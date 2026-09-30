import { adopt } from './sheets.js';
import { stopIntentProblem } from './execution-history.js';
import {
  element, eventAgent, eventSummary, firstText, foldCoordinationEvents, isObject,
  normalizedAgentState, normalizedRunState, normalizeEvent, short,
} from './coordination-turn.js';
import './session-graph-edit.js';
import './session-grants.js';

/**
 * `<ax-activation-inspector>` and the control-plane projection it shares with
 * the execution graph.
 */

const notRecorded = () => ({ status: 'not_recorded' });
const recorded = (value) => ({ status: 'available', value });
const evidenceValue = (entry, fallback = null) => (
  ['available', 'truncated'].includes(entry?.status) ? entry.value : fallback
);
const controlRequestAvailable = capability => capability?.enabled === true || capability?.requires_revalidation === true;
const unavailableControls = () => Object.fromEntries(['stop', 'retry', 'guide', 'revise']
  .map((kind) => [kind, { enabled: false, reason: 'This execution has no active command controller.' }]));

// Versioned responses never fall through to legacy event inference. This
// display validation protects the fold from unsupported/malformed shapes; the
// daemon still owns command authorization and exact runtime legality.
function controlPlaneFormatProblem(source) {
  if (source.schema_version !== 1 || !['legacy_v1', 'execution_v2'].includes(source.history_version)) {
    return 'This execution evidence format is unsupported. Refresh with a compatible Axocoatl version.';
  }
  const text = value => typeof value === 'string' && value.length > 0;
  const strings = value => Array.isArray(value) && value.every(item => typeof item === 'string');
  const reference = (value, nodeId) => {
    if (!isObject(value)) return false;
    const exact = value.kind === 'exact';
    if (exact !== (source.history_version === 'execution_v2') || (!exact && value.kind !== 'legacy')) return false;
    const identity = exact ? value.activation : value;
    return isObject(identity) && identity.session_id === source.session_id
      && identity.turn_id === source.turn_id && identity.node_id === nodeId
      && (exact ? text(identity.execution_epoch_id) && text(identity.activation_id)
        && Number.isInteger(identity.generation) && identity.generation > 0
        : identity.generation === null || (Number.isInteger(identity.generation) && identity.generation > 0));
  };
  const malformed = !text(source.session_id) || !text(source.turn_id) || !text(source.state)
    || !Array.isArray(source.nodes) || !Array.isArray(source.edges) || !strings(source.warnings)
    || source.edges.some(edge => !isObject(edge) || !text(edge.kind) || !text(edge.source) || !text(edge.target))
    || source.nodes.some(node => !isObject(node) || !text(node.node_id) || !strings(node.dependencies)
      || !Array.isArray(node.activations) || node.activations.some(activation => !isObject(activation)
        || !text(activation.state) || !reference(activation.reference, node.node_id)
        || !Array.isArray(activation.partial_outputs) || !Array.isArray(activation.evidence)
        || activation.evidence.some(event => !isObject(event) || !text(event.kind))))
    || new Set(source.nodes.map(node => node.node_id)).size !== source.nodes.length;
  if (malformed) return 'Execution evidence is malformed and cannot be displayed. Refresh this turn.';
  const recoveryCapabilities = [
    ...source.nodes.flatMap(node => node.activations.flatMap(activation =>
      ['stop', 'retry', 'guide', 'revise'].map(kind => activation.capabilities?.[kind]))),
    source.turn_controls?.continue_turn, source.turn_controls?.finish, source.turn_controls?.partial_finish?.capability,
    ...(Array.isArray(source.turn_controls?.continuation_choices) ? source.turn_controls.continuation_choices : []).map(item => item.capability),
    ...(Array.isArray(source.turn_controls?.check_choices) ? source.turn_controls.check_choices : []).map(item => item.capability),
  ].filter(Boolean);
  if (recoveryCapabilities.some(capability => capability.requires_revalidation !== undefined
      && (typeof capability.requires_revalidation !== 'boolean'
        || (capability.requires_revalidation && (capability.enabled !== false
          || source.history_version !== 'execution_v2' || source.state !== 'needs_attention'
          || source.superseded_conversation === true || !text(source.turn_controls?.execution_epoch_id)
          || typeof capability.reason !== 'string' || !capability.reason))))) {
    return 'Recovery request evidence is malformed. Refresh this turn.';
  }
  const blockers = new Set();
  for (const node of source.nodes) for (const activation of node.activations) {
    const responses = activation.capabilities?.human_responses;
    if (responses === undefined) continue;
    if (source.history_version !== 'execution_v2' || !Array.isArray(responses)) return 'Human response evidence is malformed.';
    for (const response of responses) {
      const state = response?.state?.state; const display = response?.display;
      const capability = value => isObject(value) && typeof value.enabled === 'boolean' && typeof value.reason === 'string';
      if (!text(response?.blocker_id) || blockers.has(response.blocker_id) || !text(response.request)
          || !['pending', 'resolved', 'interrupted', 'abandoned', 'replaced', 'closed'].includes(state)
          || !isObject(display) || !text(display.approval_id) || !text(display.server) || !text(display.tool)
          || typeof display.tool_display !== 'string' || typeof display.arguments_preview !== 'string'
          || !capability(response.approve) || !capability(response.decline)
          || (state !== 'pending' && (response.approve.enabled || response.decline.enabled))) return 'Human response evidence is malformed.';
      blockers.add(response.blocker_id);
    }
  }
  if (Object.hasOwn(source, 'stop_requested')) {
    if (source.history_version !== 'execution_v2') return 'Stop request evidence requires exact native history.';
    return stopIntentProblem(source.stop_requested, {revision: evidenceValue(source.turn_revision), state: source.state,
      nodes: source.nodes.map(node => node.node_id),
      startedNodes: source.nodes.filter(node => node.activations.some(activation =>
        ['running', 'accepted', 'completed', 'succeeded', 'failed', 'interrupted'].includes(activation.state))).map(node => node.node_id)});
  }
  return null;
}

function unsupportedControlPlane(source, reason) {
  return { turnId: typeof source?.turn_id === 'string' ? source.turn_id : '',
    sessionId: typeof source?.session_id === 'string' ? source.session_id : '',
    schemaVersion: null, historyVersion: null, status: 'unavailable', unsupported: true,
    request: '', agents: [], nodes: [], agentCount: 0, answers: [], answer: '', handoffs: [], timeline: [],
    recoveryEvidence: '', hasCoordination: false, warnings: [reason], controlPlane: null };
}

/**
 * The card, execution graph, and inspector share this transport-free projection.
 * Accepts the versioned GET control-plane envelope or a retained v1 Session turn.
 * A missing generation stays unknown; a current Settings definition is never
 * substituted for the definition actually used by historical work.
 */
export function foldControlPlane(source = null, events = null) {
  if (source != null && !isObject(source)) {
    return unsupportedControlPlane(source, 'Execution evidence is malformed and cannot be displayed. Refresh this turn.');
  }
  const isEnvelope = isObject(source) && ['schema_version', 'history_version', 'nodes']
    .some(key => Object.hasOwn(source, key));
  if (isEnvelope) {
    const problem = controlPlaneFormatProblem(source);
    if (problem) return unsupportedControlPlane(source, problem);
  }
  if (!isEnvelope) {
    const turn = isObject(source) ? source : {};
    const entries = events ?? turn.execution_events ?? [];
    const legacy = foldCoordinationEvents(entries, turn);
    const normalized = entries.map(normalizeEvent).filter(Boolean);
    if (!legacy.agents.length && typeof turn.agent_id === 'string' && turn.agent_id) {
      legacy.agents = [{ id: turn.agent_id, label: turn.agent_id, dependsOn: [],
        state: normalizedAgentState(turn.status, turn.status || 'unknown'), summary: turn.error || '' }];
      legacy.agentCount = 1;
    }
    const nodes = legacy.agents.map((agent) => {
      const own = normalized.filter((event) => eventAgent(event.metadata) === agent.id);
      const outputs = (turn.agent_outputs || []).filter((output) => output.agent_id === agent.id);
      const generations = [...new Set([
        ...own.map((event) => event.metadata.generation ?? event.metadata.activation_generation ?? null),
        ...outputs.map((output) => output.activation_generation ?? null),
      ])].sort((a, b) => (a ?? -1) - (b ?? -1));
      if (!generations.length) generations.push(null);
      return {
        node_id: agent.id, definition_id: agent.id, label: agent.label,
        definition: notRecorded(), dependencies: agent.dependsOn,
        activations: generations.map((generation) => {
          const related = own.filter((event) =>
            (event.metadata.generation ?? event.metadata.activation_generation ?? null) === generation);
          const output = outputs.filter((item) => (item.activation_generation ?? null) === generation).at(-1);
          const last = related.at(-1);
          const started = related.find((event) => event.kind === 'coordination_agent_activated');
          const completed = related.findLast((event) => /agent_(completed|failed|cancelled)$/.test(event.kind));
          const state = last?.kind === 'coordination_agent_completed' ? 'completed'
            : last?.kind === 'coordination_agent_failed' ? 'failed'
              : last?.kind === 'coordination_agent_cancelled' ? 'stopped'
                : last?.kind === 'coordination_agent_blocked' ? 'blocked'
                  : last?.kind === 'coordination_agent_activated' ? 'working'
                    : last?.kind === 'agent_output_superseded' ? 'superseded' : agent.state;
          const time = (event) => event && Number.isFinite(event.recordedAt)
            && event.recordedAt > 100000000000 ? recorded(event.recordedAt) : notRecorded();
          return {
            reference: { kind: 'legacy', session_id: turn.session_id || '', turn_id: legacy.turnId, node_id: agent.id, generation },
            generation: generation === null ? notRecorded() : recorded(generation),
            state: output?.superseded ? 'superseded' : turn.status === 'interrupted'
              && ['working', 'waiting'].includes(state) ? 'interrupted' : state,
            reason: last ? recorded(eventSummary(last.metadata, last.kind))
              : turn.agent_id === agent.id && turn.error ? recorded(turn.error) : notRecorded(),
            started_at: time(started), completed_at: time(completed), input: notRecorded(),
            output: output ? recorded(firstText(output.output, output.content))
              : turn.agent_id === agent.id && typeof turn.final_output === 'string' ? recorded(turn.final_output) : notRecorded(),
            partial_outputs: turn.agent_id === agent.id && turn.partial_output
              ? [{ text: turn.partial_output, truncated: false }] : [], usage: notRecorded(), capabilities: { inspect: true, ...unavailableControls() },
            evidence: related.map((event) => ({
              kind: event.kind, reference: event.recordedOperationId ? recorded(event.recordedOperationId) : notRecorded(),
              summary: recorded(eventSummary(event.metadata, event.kind)),
              recorded_at: time(event), details: recorded(event.metadata),
            })),
          };
        }),
      };
    });
    return { ...legacy, nodes, sessionId: turn.session_id || '', historyVersion: 'legacy_v1',
      schemaVersion: 1, warnings: [], controlPlane: null };
  }
  const nodes = source.nodes;
  const stopRequested = source.stop_requested ?? null;
  const unrunNodes = new Set(stopRequested?.unrun_nodes || []);
  // A helper admitted through delegate names its lead. It is kept apart from
  // dependsOn: the lead waits on it inside one activation, so it neither
  // blocks the lead nor becomes the turn's answer.
  const delegations = (source.edges || []).filter((edge) => edge.kind === 'delegated_by');
  const agents = nodes.map((node) => {
    const activation = node.activations.at(-1);
    return { id: node.node_id, label: node.label || node.node_id,
      dependsOn: node.dependencies || [],
      delegatedBy: [...new Set(delegations.filter((edge) => edge.target === node.node_id).map((edge) => edge.source))],
      state: unrunNodes.has(node.node_id) ? 'stopped'
        : normalizedAgentState(activation?.state, activation?.state || 'waiting'),
      summary: unrunNodes.has(node.node_id) ? (stopRequested?.partial_finish ? 'Skipped by partial Finish' : 'Stopped before starting') : evidenceValue(activation?.reason, '') };
  });
  if (source.history_version === 'execution_v2') {
    const byId = new Map(agents.map(agent => [agent.id, agent]));
    const neverStarted = new Set(nodes.filter(node => !node.activations.length).map(node => node.node_id));
    const blockedStates = new Set(['failed', 'blocked', 'interrupted', 'stopped']);
    // A node need not have an activation to be unable to run. Derive only its
    // display status from current dependency states; retain the empty history.
    let changed;
    do {
      changed = false;
      for (const agent of agents) {
        if (!neverStarted.has(agent.id) || agent.state !== 'waiting') continue;
        const blockers = agent.dependsOn.map(id => byId.get(id)).filter(parent => blockedStates.has(parent?.state));
        if (blockers.length) {
          agent.state = 'blocked'; agent.summary = `Blocked by ${blockers.map(parent => parent.label).join(', ')}.`;
          changed = true;
        }
      }
    } while (changed);
  }
  const sinks = new Set(agents.filter((node) => !node.delegatedBy.length
    && !agents.some((other) => other.dependsOn.includes(node.id))).map((node) => node.id));
  const answers = nodes.flatMap((node) => {
    const activation = node.activations.at(-1);
    const content = evidenceValue(activation?.output);
    if (!sinks.has(node.node_id) || !content || !['completed', 'succeeded', 'accepted'].includes(activation?.state)) return [];
    if (stopRequested?.partial_finish && !stopRequested.partial_finish.selected_activations.some(item => activationSelectionKey({kind:'exact',activation:item}) === activationSelectionKey(activation.reference))) return [];
    return [{ agentId: node.node_id, label: node.label || node.node_id, content, generation: evidenceValue(activation.generation) }];
  });
  return {
    turnId: source.turn_id, sessionId: source.session_id,
    schemaVersion: source.schema_version, historyVersion: source.history_version,
    request: evidenceValue(source.request, ''), status: normalizedRunState(source.state, source.state),
    agents, nodes, stopRequested, agentCount: nodes.length, answers, answer: answers.map((a) => a.content).join('\n\n'),
    handoffs: (source.edges || []).filter((edge) => edge.kind !== 'dependency').map((edge) => ({
      from: edge.source, to: edge.target, kind: edge.kind,
      summary: edge.kind === 'delegated_by' ? `Delegated a task to helper ${evidenceValue(edge.summary, 'not recorded')}`
        : evidenceValue(edge.summary, 'Not recorded'),
    })),
    timeline: nodes.flatMap((node) => node.activations.flatMap((activation) => (activation.evidence || []).map((event) => ({
      kind: event.kind, label: `${node.label || node.node_id} · ${event.kind.replaceAll('_', ' ')}`,
      summary: evidenceValue(event.summary, ''), recordedAt: evidenceValue(event.recorded_at), state: activation.state,
    })))).sort((a, b) => (a.recordedAt ?? 0) - (b.recordedAt ?? 0)),
    recoveryEvidence: '', hasCoordination: nodes.length > 0,
    warnings: source.warnings || [], controlPlane: source,
  };
}

const INSPECTOR_CSS = `
:host { display: block; min-width: 0; color: var(--text); font: var(--fs-body,13.5px)/1.55 var(--font-sans,sans-serif); }
:host([hidden]) { display: none; }
* { box-sizing: border-box; }
dialog { position: static; inset: auto; display: block; margin: 0; padding: 0; width: 100%; max-width: none; max-height: none; border: 1px solid var(--border); border-radius: var(--r-lg,10px); color: inherit; background: var(--panel); }
dialog:not([open]) { display: none; }
.content { padding: var(--sp-4,16px); overflow-wrap: anywhere; }
header { display: flex; align-items: start; justify-content: space-between; gap: 12px; }
h2 { margin: 0; font-size: var(--fs-title,16px); }
h3 { font-size: var(--fs-sm,12.5px); margin: 20px 0 8px; }
p { margin: 6px 0; }
button,select { font: inherit; color: inherit; background: var(--bg-3); border: 1px solid var(--border-strong); border-radius: var(--r-md,6px); padding: 5px 8px; max-width: 100%; }
button { cursor: pointer; }
button:focus-visible,select:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
.close { flex: 0 0 auto; }
.label,.unavailable { color: var(--muted); font-size: var(--fs-xs,11px); }
dl { margin: 0; display: grid; gap: 5px 12px; grid-template-columns: minmax(75px,auto) minmax(0,1fr); }
dt { color: var(--muted); } dd { margin: 0; }
pre { max-height: 230px; overflow: auto; white-space: pre-wrap; font: var(--fs-xs,11px)/1.5 var(--font-mono,monospace); background: var(--bg-3); padding: 8px; border-radius: 6px; }
.evidence { padding-top: 8px; margin-top: 8px; border-top: 1px solid var(--border); }
.notice { color: var(--warn); }
.actions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 16px; }
.control-form { display: grid; gap: 8px; padding: 10px 0; }
.control-form label { display: flex; gap: 8px; align-items: start; }
.control-form textarea { box-sizing: border-box; width: 100%; min-height: 90px; resize: vertical; background: var(--bg); color: var(--text); border: 1px solid var(--border); border-radius: 6px; padding: 8px; font: inherit; }
.control-form button { justify-self: start; }
.required-check { padding: 8px 0; border-top: 1px solid var(--border); }
.required-check .check-state { font-weight: 600; }
.required-check.failed .check-state,.required-check.timed_out .check-state,.required-check.signalled .check-state,.required-check.launch_failed .check-state { color: var(--err, var(--warn)); }
.check-readiness { padding: 8px 0; border-top: 1px solid var(--border); }
.check-readiness .check-readiness-state { font-weight: 600; margin: 0; }
.check-readiness.passed .check-readiness-state { color: var(--ok, var(--text)); }
.check-readiness.failed .check-readiness-state,.check-readiness.unavailable .check-readiness-state { color: var(--err, var(--warn)); }
.check-readiness .check-readiness-reason { margin: 4px 0 0; }
dialog:modal { position: fixed; inset: 12px; width: calc(100% - 24px); max-height: calc(100dvh - 24px); margin: auto; overflow: auto; }
dialog::backdrop { background: rgba(0,0,0,.55); }
`;

function evidenceText(value) {
  if (['available', 'truncated'].includes(value?.status)) {
    const content = typeof value.value === 'string' ? value.value : JSON.stringify(value.value, null, 2);
    return `${content ?? 'Not recorded'}${value.status === 'truncated' ? '\n[Truncated recorded evidence]' : ''}`;
  }
  if (value?.status === 'unknown') return `Unknown${value.reason ? ` · ${value.reason}` : ''}`;
  if (value?.status === 'missing') return `Missing recorded evidence${value.reference ? ` · ${value.reference}` : ''}`;
  if (value?.status === 'unavailable') return `Unavailable${value.reason ? ` · ${value.reason}` : ''}`;
  return 'Not recorded';
}

// A check's argv as a shell reads it: a `sh -c` script as typed, any other
// argv with each argument quoted when it needs to be.
function shellWord(word) {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(word) ? word : `'${word.replaceAll("'", "'\\''")}'`;
}
function checkCommand(argv) {
  return argv.length === 3 && argv[0] === 'sh' && argv[1] === '-c' ? argv[2] : argv.map(shellWord).join(' ');
}

// Required-check conditions are `required-check:0` (capture before), `:1..n`
// (the commands), `:n+1` (capture after) and `:ready`.
function checkChoiceLabel(conditionId, requiredChecks) {
  const match = /^required-check:(\d+|ready)$/.exec(conditionId || '');
  const checks = Array.isArray(requiredChecks) ? requiredChecks : [];
  if (!match || !checks.length) return `Check · ${conditionId}`;
  if (match[1] === 'ready') return 'Readiness of the required checks';
  const index = Number(match[1]);
  if (index === 0) return 'Repository capture before the required checks';
  if (index === checks.length + 1) return 'Repository capture after the required checks';
  const argv = checks[index - 1]?.argv;
  return Array.isArray(argv) ? `Required check · ${checkCommand(argv)}` : `Check · ${conditionId}`;
}

function activationSelectionKey(reference) {
  const value = reference?.kind === 'exact' ? reference.activation : reference;
  return JSON.stringify([value?.session_id, value?.turn_id, value?.execution_epoch_id,
    value?.node_id, value?.generation, value?.activation_id]);
}

/** Minimal selected-activation evidence; transport and command authority belong to the shell. */
export class AxActivationInspector extends HTMLElement {
  #root;
  #dialog;
  #content;
  #model = null;
  #nodeId = '';
  #generation = null;
  #activationKey = '';
  #followLatest = true;
  #narrow = window.matchMedia('(max-width: 720px)');
  #returnFocus = null;
  #commandHistory = [];
  #suspended = false;
  #turnMode = false;
  #revisionDrafts = new Map();
  #continuationDraft = { restart: new Set(), checks: new Set() };
  #graphEditor = document.createElement('ax-session-graph-edit');
  #grants = document.createElement('ax-session-grants');
  get suspended() { return this.#suspended; }
  set suspended(value) { this.#suspended = Boolean(value); this.#syncPresentation(); }

  get commandHistory() { return this.#commandHistory; }
  set commandHistory(value) { this.#commandHistory = Array.isArray(value) ? value : []; this.#render(); }

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, INSPECTOR_CSS);
    this.#dialog = document.createElement('dialog');
    this.#dialog.setAttribute('aria-label', 'Agent activation details');
    this.#content = element('div', 'content');
    this.#dialog.append(this.#content);
    this.#root.append(this.#dialog, this.#graphEditor, this.#grants);
    this.#dialog.addEventListener('cancel', (event) => { event.preventDefault(); this.close(); });
    this.#dialog.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      event.stopPropagation();
      this.close();
    });
  }

  connectedCallback() { this.#narrow.addEventListener('change', this.#syncPresentation); this.#render(); }
  disconnectedCallback() { this.#narrow.removeEventListener('change', this.#syncPresentation); this.#dialog.close(); }
  get model() { return this.#model; }
  set model(value) {
    if (this.#model?.turnId !== value?.turnId || this.#model?.sessionId !== value?.sessionId) {
      this.#generation = null; this.#activationKey = ''; this.#followLatest = true;
      this.#turnMode = false; this.#revisionDrafts.clear();
      this.#graphEditor.close(true);this.#grants.close();
      this.#continuationDraft = { restart: new Set(), checks: new Set() };
    }
    this.#model = value;
    this.#render();
  }
  get nodeId() { return this.#nodeId; }
  showTurnControls() { this.#rememberReturnFocus(); this.#turnMode = true; this.#nodeId = ''; this.#render(); this.focusInspector(); }
  set nodeId(value) {
    if (value && this.hidden) this.#rememberReturnFocus();
    if (value) this.#turnMode = false;
    if (this.#nodeId !== value) { this.#generation = null; this.#activationKey = ''; this.#followLatest = true; }
    this.#nodeId = value || ''; this.#render();
  }
  set activationReference(value) { this.#activationKey = activationSelectionKey(value); this.#followLatest = false; this.#render(); }
  get generation() { return this.#generation; }
  set generation(value) { this.#generation = value; this.#activationKey = ''; this.#followLatest = false; this.#render(); }

  focusInspector() {
    this.#rememberReturnFocus();
    this.#root.querySelector('.close')?.focus();
  }
  #rememberReturnFocus() {
    if (this.#returnFocus?.isConnected || this.matches(':focus-within')) return;
    let active = document.activeElement;
    while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
    this.#returnFocus = active;
  }
  close() {
    this.#turnMode = false;
    this.#dialog.close();
    this.hidden = true;
    const returnFocus = this.#returnFocus;
    this.#returnFocus = null;
    if (returnFocus?.isConnected) returnFocus.focus?.();
    this.dispatchEvent(new CustomEvent('close-inspector', { bubbles: true, composed: true,
      detail: { focusRestored: Boolean(returnFocus?.isConnected && returnFocus.matches(':focus')) } }));
  }
  #syncPresentation = () => {
    if (this.#suspended) { this.#dialog.close(); return; }
    if (!this.isConnected || this.hidden) return;
    const isModal = this.#dialog.matches(':modal');
    if (!this.#dialog.open) this.#rememberReturnFocus();
    if (this.#narrow.matches && !isModal) { this.#dialog.close(); this.#dialog.showModal(); }
    else if (!this.#narrow.matches && (isModal || !this.#dialog.open)) {
      this.#dialog.close(); this.#dialog.show();
    }
  };
  #row(list, label, value) {
    list.append(element('dt', '', label), element('dd', value?.status === 'available' ? '' : 'unavailable', evidenceText(value)));
  }
  #block(label, value) {
    this.#content.append(element('h3', '', label));
    if (value?.status === 'available' && value.value === '') {
      this.#content.append(element('p', 'label', label === 'Output' ? 'Recorded empty output' : `Recorded empty ${label.toLowerCase()}`));
      return;
    }
    const available = ['available', 'truncated'].includes(value?.status);
    this.#content.append(element(available ? 'pre' : 'p', available ? '' : 'unavailable', evidenceText(value)));
  }
  #referenceButton(item, selected, node, type = 'event', label = 'Add to chat') {
    const referenceId = evidenceValue(item.reference);
    const exact = selected.reference?.kind === 'exact' ? selected.reference.activation : null;
    const sessionId = this.#model?.sessionId;
    const turnId = this.#model?.turnId;
    if (typeof referenceId !== 'string' || !referenceId || !sessionId || !turnId) return null;
    if (type === 'output' && !['available', 'truncated'].includes(selected.output?.status)) return null;
    if (this.#model.historyVersion === 'execution_v2' && !exact) return null;
    const generation = evidenceValue(selected.generation);
    const name = `${node.label || node.node_id} · ${type}${generation === null ? '' : ` · generation ${generation}`}${selected.state === 'superseded' ? ' · superseded' : ''}${this.#model?.controlPlane?.superseded_conversation ? ' · superseded conversation' : ''}`;
    const button = element('button', 'add-reference', label); button.type = 'button';
    button.setAttribute('aria-label', `Add ${name} to chat`);
    button.addEventListener('click', () => {
      const metadata = { history_version: this.#model.historyVersion, source_session_id: sessionId,
        source_turn_id: turnId, node_id: node.node_id, generation, reference_id: referenceId, type };
      if (exact) { metadata.execution_epoch_id = exact.execution_epoch_id; metadata.activation_id = exact.activation_id; }
      this.dispatchEvent(new CustomEvent('attach-coordination-reference', {
        detail: { reference: { reference_id: referenceId, display_name: name, kind: 'coordination_reference',
          scope: 'this_turn', origin: null, metadata }, preview: short(evidenceValue(item.summary, ''), 240) },
        bubbles: true, composed: true,
      }));
    });
    return button;
  }
  #renderReceipts(commands) {
    if (!commands.length) return;
    this.#content.append(element('h3', '', 'Commands'));
    const labels = { requested: 'Requested · awaiting validation', accepted: 'Accepted · waiting for application',
      applied: 'Applied · waiting for a safe boundary', settled: 'Settled', rejected: 'Rejected', failed: 'Failed' };
    const names = { stop: 'Stop', retry: 'Retry', guide: 'Guide', resume: 'Human response', revise: 'Revise', continue: 'Continue', finish: 'Finish' };
    for (const command of commands) {
      const item = element('div', 'command-receipt'); item.setAttribute('role', 'status');
      const state = command.receipt?.state;
      const status = command.sending ? 'Sending · receipt not yet confirmed'
        : command.error ? 'Outcome unknown · receipt could not be confirmed'
          : command.request.action === 'resume' && state === 'settled' ? 'Response received by the Agent'
          : command.request.action === 'guide' && state === 'settled' ? 'Input received by the Agent'
            : labels[state] || 'Receipt unavailable';
      const commandName = command.request.partial_finish || command.receipt?.request?.parameters?.mode?.kind === 'force_partial'
        ? 'Finish partial result' : names[command.request.action] || 'Command';
      item.append(element('p', '', `${commandName} · ${status}`));
      const reason = command.receipt?.last_transition?.failure?.message;
      if (reason) item.append(element('p', 'notice', reason));
      if (command.error) item.append(element('p', 'notice', command.error));
      item.append(element('p', 'label', `Command ${command.request.command_id}`));
      if (!command.sending && !['settled', 'rejected', 'failed'].includes(state)
          && (typeof this.receiptRefreshHandler === 'function' || (command.canResubmit !== false && typeof this.commandHandler === 'function'))) {
        const check = element('button', 'check-command', 'Check command status'); check.type = 'button';
        check.addEventListener('click', async () => {
          try {
            if (command.receipt && typeof this.receiptRefreshHandler === 'function') await this.receiptRefreshHandler();
            else if (command.canResubmit !== false && typeof this.commandHandler === 'function') await this.commandHandler({ request: command.request, model: this.#model });
            else if (typeof this.receiptRefreshHandler === 'function') await this.receiptRefreshHandler();
          } catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
        });
        item.append(check);
      }
      this.#content.append(item);
    }
  }
  #renderHumanResponses(selected, commands) {
    for (const response of selected.capabilities?.human_responses || []) {
      const form = element('form', 'control-form human-response-form');
      form.dataset.blockerId = response.blocker_id; form.dataset.evidenceRef = response.request;
      const display = response.display;
      form.append(element('h3', '', 'This work needs your decision'),
        element('p', 'human-response-tool', `${display.tool_display || display.tool} · ${display.server}`),
        element('p', 'label', 'Arguments preview'), element('pre', 'human-response-arguments', display.arguments_preview));
      const state = response.state.state;
      if (state !== 'pending' || this.#model?.stopRequested || (!response.approve.enabled && !response.decline.enabled)) {
        const labels = {resolved: 'Response recorded', interrupted: 'The previous execution was interrupted. This wait cannot be resumed.',
          abandoned: 'This wait ended without a human decision.', replaced: 'This work was replaced.', closed: 'This turn is closed.'};
        form.append(element('p', 'unavailable', labels[state] || response.approve.reason || response.decline.reason || 'This wait cannot be resumed.'));
        this.#content.append(form); continue;
      }
      const key = `resume:${activationSelectionKey(selected.reference)}:${response.blocker_id}`;
      const draft = this.#revisionDrafts.get(key) || {reason: ''}; this.#revisionDrafts.set(key, draft);
      const reason = element('textarea', 'human-response-reason'); reason.setAttribute('aria-label', 'Reason for declining');
      reason.placeholder = 'Reason for declining'; reason.value = draft.reason;
      const approve = element('button', 'resume-approve', 'Allow once'); approve.type = 'button';
      const decline = element('button', 'resume-decline', 'Decline'); decline.type = 'submit';
      const pending = commands.some(command => command.request.action === 'resume' && command.request.blocker_id === response.blocker_id
        && !['settled', 'rejected', 'failed'].includes(command.receipt?.state));
      let sending = false;
      const sync = () => { approve.disabled = pending || sending || !response.approve.enabled;
        decline.disabled = pending || sending || !response.decline.enabled || !reason.value.trim(); };
      reason.addEventListener('input', () => {draft.reason = reason.value; sync();});
      const send = async humanResponse => {
        sending = true; sync();
        try { await this.commandHandler({kind: 'resume', reference: selected.reference, model: this.#model,
          blockerId: response.blocker_id, humanResponse}); }
        catch (error) {this.#content.append(element('p', 'notice', error.message || String(error)));}
        finally {sending = false; sync();}
      };
      approve.addEventListener('click', () => {if (!approve.disabled) void send({kind: 'approval'});});
      form.addEventListener('submit', event => {event.preventDefault(); if (!decline.disabled) void send({kind: 'decline', reason: draft.reason});});
      form.append(element('p', 'label', 'Your response applies to this exact tool request. Allowing it does not grant future permission.'), reason, approve, decline);
      if (!response.approve.enabled) form.append(element('p', 'unavailable', response.approve.reason));
      if (!response.decline.enabled) form.append(element('p', 'unavailable', response.decline.reason));
      sync(); this.#content.append(form);
    }
  }
  #renderGuidance(selected, commands) {
    if (this.#model?.stopRequested || selected.capabilities?.guide?.enabled !== true) return;
    const key = `guide:${activationSelectionKey(selected.reference)}`;
    const draft = this.#revisionDrafts.get(key) || { instruction: '' };
    this.#revisionDrafts.set(key, draft);
    const form = element('form', 'control-form guidance-form');
    form.append(element('h3', '', 'Guide this work'), element('p', 'label',
      'Adds your instruction when the Agent reaches its next safe boundary. Follow its output for the result.'));
    const instruction = element('textarea', 'guidance-instruction');
    instruction.setAttribute('aria-label', 'Guidance instruction');
    instruction.required = true; instruction.value = draft.instruction;
    const submit = element('button', 'guide', 'Send guidance'); submit.type = 'submit';
    const pending = commands.some(item => item.request.action === 'guide'
      && !['settled', 'rejected', 'failed'].includes(item.receipt?.state));
    const sync = () => { submit.disabled = pending || !instruction.value.trim(); };
    instruction.addEventListener('input', () => { draft.instruction = instruction.value; sync(); });
    sync(); form.append(instruction, submit);
    form.addEventListener('submit', async event => {
      event.preventDefault(); if (submit.disabled) return; submit.disabled = true;
      try { await this.commandHandler({ kind: 'guide', reference: selected.reference, model: this.#model,
        instruction: draft.instruction }); }
      catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
      finally { sync(); }
    });
    this.#content.append(form);
  }
  #renderRevision(selected, commands) {
    if (!controlRequestAvailable(selected.capabilities?.revise)) return;
    const key = activationSelectionKey(selected.reference);
    const draft = this.#revisionDrafts.get(key) || { instruction: '', includePreviousOutput: false };
    this.#revisionDrafts.set(key, draft);
    const form = element('form', 'control-form revision-form');
    form.append(element('h3', '', 'Revise this result'));
    if (selected.capabilities.revise.requires_revalidation) form.append(element('p', 'recovery-notice', selected.capabilities.revise.reason));
    const instruction = element('textarea', 'revision-instruction'); instruction.setAttribute('aria-label', 'Revision instruction');
    instruction.required = true; instruction.value = draft.instruction;
    const include = element('input', 'revision-context'); include.type = 'checkbox'; include.checked = draft.includePreviousOutput;
    const label = element('label'); label.append(include, document.createTextNode('Include the previous answer as context'));
    const affected = selected.capabilities.revise_invalidates || [];
    form.append(element('p', 'label', 'Creates a new generation. The accepted result remains in history.'));
    form.append(element('p', 'revision-impact', affected.length
      ? `Invalidates dependent results: ${affected.map(item => `${item.node_id} · generation ${item.generation}`).join(', ')}.`
      : 'No recorded dependent generations are invalidated.'));
    const submit = element('button', 'revise', 'Revise'); submit.type = 'submit';
    const pending = commands.some(item => item.request.action === 'revise' && !['settled', 'rejected', 'failed'].includes(item.receipt?.state));
    const sync = () => { submit.disabled = pending || !instruction.value.trim(); };
    instruction.addEventListener('input', () => { draft.instruction = instruction.value; sync(); });
    include.addEventListener('change', () => { draft.includePreviousOutput = include.checked; });
    sync(); form.append(instruction, label, submit);
    form.addEventListener('submit', async event => {
      event.preventDefault(); if (submit.disabled) return; submit.disabled = true;
      try { await this.commandHandler({ kind: 'revise', reference: selected.reference, model: this.#model,
        instruction: draft.instruction, includePreviousOutput: draft.includePreviousOutput }); }
      catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
      finally { sync(); }
    });
    this.#content.append(form);
  }
  // What the latest run of each of the Session team's required checks shows.
  // Recorded output is retained evidence, shown as bounded previews.
  #renderRequiredChecks() {
    const checks = this.#model?.controlPlane?.required_checks;
    if (!Array.isArray(checks) || !checks.length) return;
    this.#content.append(element('h3', '', 'Required checks'), element('p', 'label',
      'The host runs these after the required Agents finish. A failure leaves the turn needing attention; select a check below and Continue to run them all again.'));
    // Whether the checks passed together on the current tree: a command's own
    // exit code alone does not make the turn ready.
    const readiness = this.#model?.controlPlane?.required_check_readiness;
    if (readiness && typeof readiness.state === 'string') {
      const titles = {passed: 'Ready · every check passed on the current tree', failed: 'Not ready',
        not_run: 'Not run yet', skipped: 'Skipped', unavailable: 'Readiness unavailable'};
      const summary = element('section', `check-readiness ${readiness.state}`);
      summary.append(element('p', 'check-readiness-state', titles[readiness.state] || readiness.state));
      if (readiness.state !== 'passed' && typeof readiness.reason === 'string' && readiness.reason)
        summary.append(element('p', 'check-readiness-reason', readiness.reason));
      this.#content.append(summary);
    }
    const states = {pending: 'Not run yet', passed: 'Passed', failed: 'Failed', unverified: 'Exited 0, not yet counted as passing',
      signalled: 'Stopped by a signal', timed_out: 'Timed out', interrupted: 'Interrupted', launch_failed: 'Could not start',
      not_dispatched: 'Not started', outcome_unknown: 'Outcome unknown', skipped: 'Skipped', unavailable: 'Unavailable'};
    for (const check of checks) {
      if (!Array.isArray(check?.argv)) continue;
      const item = element('section', `required-check ${typeof check.state === 'string' ? check.state : ''}`);
      item.append(element('pre', 'check-command', check.argv.length ? checkCommand(check.argv) : 'Command unavailable'));
      const state = element('p', 'check-state', `${states[check.state] || String(check.state || 'Unknown')}${Number.isInteger(check.exit_code) ? ` · exit code ${check.exit_code}` : ''}`);
      item.append(state);
      if (typeof check.reason === 'string' && check.reason) item.append(element('p', 'label', check.reason));
      for (const [key, label] of [['stdout', 'Output'], ['stderr', 'Errors']]) {
        if (typeof check[key] !== 'string' || !check[key]) continue;
        item.append(element('p', 'label', `${label}${check[`${key}_truncated`] ? ' (truncated)' : ''}`), element('pre', `check-${key}`, check[key]));
      }
      this.#content.append(item);
    }
  }
  #renderTurnControls() {
    const controls = this.#model?.controlPlane?.turn_controls;
    if (typeof this.commandHandler !== 'function' || !controls) {
      this.#content.append(element('p', 'unavailable', 'Controls are unavailable for this retained turn.'));
      return;
    }
    const form = element('form', 'control-form continuation-form');
    form.append(element('h3', '', 'Continue this turn'), element('p', 'label',
      'Select the work and checks to continue. Accepted results stay retained; other unfinished work stays blocked.'));
    if (controls.continue_turn?.requires_revalidation) form.append(element('p', 'recovery-notice', controls.continue_turn.reason));
    const draft = this.#continuationDraft;
    const choices = [];
    for (const [kind, items] of [['restart', controls.continuation_choices || []], ['checks', controls.check_choices || []]]) {
      for (const item of items) {
        const key = kind === 'restart' ? activationSelectionKey({kind: 'exact', activation: item.activation}) : item.condition_id;
        const checkbox = element('input', kind === 'restart' ? 'continue-work' : 'continue-check'); checkbox.type = 'checkbox';
        checkbox.disabled = !controlRequestAvailable(item.capability); checkbox.checked = !checkbox.disabled && draft[kind].has(key);
        const text = kind === 'restart' ? `${item.activation.node_id} · generation ${item.activation.generation} · ${item.state}` : checkChoiceLabel(item.condition_id, this.#model?.controlPlane?.required_checks);
        const label = element('label'); label.append(checkbox, document.createTextNode(text)); form.append(label);
        // Any required check reruns them all; that is said once below.
        if (kind === 'checks' && item.required_conditions?.length && !/^required-check:/.test(item.condition_id || ''))
          form.append(element('p', 'continuation-dependencies', `Also runs again: ${item.required_conditions.join(', ')}.`));
        if (checkbox.disabled) form.append(element('p', 'unavailable', item.capability?.reason || 'Unavailable'));
        choices.push({kind, item, checkbox});
        checkbox.addEventListener('change', () => { if (checkbox.checked) draft[kind].add(key); else draft[kind].delete(key); sync(); });
      }
    }
    if ((controls.check_choices || []).some(item => /^required-check:/.test(item.condition_id || '')))
      form.append(element('p', 'continuation-dependencies',
        'Any required check you select runs every required check again between fresh repository captures, then records readiness.'));
    if ((controls.continuation_choices || []).length && this.#model?.controlPlane?.required_checks?.length)
      form.append(element('p', 'continuation-checks', 'Restarted work runs the required checks again after it finishes.'));
    const pending = this.#commandHistory.some(item => ['continue', 'finish'].includes(item.request.action)
      && !['settled', 'rejected', 'failed'].includes(item.receipt?.state));
    const submit = element('button', 'continue-turn', 'Continue'); submit.type = 'submit';
    const sync = () => { submit.disabled = pending || !controlRequestAvailable(controls.continue_turn) || !choices.some(item => item.checkbox.checked); };
    sync(); form.append(submit);
    if (!controlRequestAvailable(controls.continue_turn)) form.append(element('p', 'unavailable', controls.continue_turn?.reason || 'No executable continuation is available.'));
    form.addEventListener('submit', async event => {
      event.preventDefault(); if (submit.disabled) return; submit.disabled = true;
      const continuation = { restart: [], checks: [] };
      choices.filter(item => item.checkbox.checked).forEach(({kind,item}) => continuation[kind].push(kind === 'restart' ? item.activation : item.condition_id));
      try { await this.commandHandler({kind: 'continue', continuation, model: this.#model}); }
      catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
      finally { sync(); }
    });
    this.#content.append(form);
    this.#content.append(element('h3', '', 'Finish this turn'), element('p', 'label',
      'Finish waits for running work and required conditions. It does not bypass unresolved effects or checks.'));
    const finish = element('button', 'finish-turn', 'Finish'); finish.type = 'button';
    finish.disabled = pending || !controlRequestAvailable(controls.finish);
    finish.addEventListener('click', async () => {
      finish.disabled = true;
      try { await this.commandHandler({kind: 'finish', model: this.#model}); }
      catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
    });
    this.#content.append(finish);
    if (controls.finish?.requires_revalidation) this.#content.append(element('p', 'recovery-notice', controls.finish.reason));
    if (!controlRequestAvailable(controls.finish)) this.#content.append(element('p', 'unavailable', controls.finish?.reason || 'Finish is unavailable.'));
    const partial = controls.partial_finish;
    if (partial) {
      const partialPending = this.#commandHistory.some(command =>
        (command.request.partial_finish || command.receipt?.request?.parameters?.mode?.kind === 'force_partial')
        && !['settled', 'rejected', 'failed'].includes(command.receipt?.state));
      const review = structuredClone(partial.review);
      const form = element('form', 'partial-finish-review');
      form.append(element('h3', '', 'Finish partial result'), element('p', 'label',
        'Stop unfinished work and close this turn as Finished. This does not make missing checks pass or undo effects. Only the accepted results you select below supply the final answer and next-turn conversation.'));
      const revision = this.#model.controlPlane.turn_revision?.value;
      if (this._partialFinishDraft?.revision !== revision) this._partialFinishDraft = {revision, selected: [], confirmed: false};
      const draft = this._partialFinishDraft;
      const choices = [];
      for (const activation of partial.available_sinks || []) {
        const label = element('label', 'partial-result-choice');
        const checkbox = element('input', 'partial-result'); checkbox.type = 'checkbox';
        checkbox.checked = draft.selected.includes(activation.activation_id);
        const node = this.#model.nodes?.find(node => node.node_id === activation.node_id);
        label.append(checkbox, document.createTextNode(` ${node?.label || activation.node_id} · accepted generation ${activation.generation}`));
        form.append(label); choices.push({activation, checkbox});
      }
      form.append(element('p', 'partial-no-result', 'Leaving every result unselected finishes with no result. Earlier committed conversation remains available.'));
      const describe = activation => `${activation.node_id} · generation ${activation.generation}`;
      for (const [title, values] of [
        ['Work to stop safely', review.stop_activations.map(describe)],
        ['Work skipped before starting', review.unrun_nodes],
        ['Missing or unmet checks', review.missing_conditions],
      ]) {
        form.append(element('h4', '', title));
        const list = element('ul');
        for (const value of values) list.append(element('li', '', value));
        if (!values.length) list.append(element('li', '', 'None'));
        form.append(list);
      }
      const confirmation = element('label', 'partial-confirmation');
      const checkbox = element('input', 'confirm-partial-finish'); checkbox.type = 'checkbox'; checkbox.checked = draft.confirmed;
      confirmation.append(checkbox, document.createTextNode(' I reviewed the selected results, stopped and skipped work, and missing checks. Finish this partial result.'));
      const submit = element('button', 'finish-partial', 'Finish partial result'); submit.type = 'submit';
      const sync = () => {
        draft.confirmed = checkbox.checked;
        draft.selected = choices.filter(item => item.checkbox.checked).map(item => item.activation.activation_id);
        submit.disabled = partialPending || !checkbox.checked || !controlRequestAvailable(partial.capability);
      };
      form.addEventListener('change', sync); sync();
      form.addEventListener('submit', async event => {
        event.preventDefault(); if (submit.disabled) return; submit.disabled = true;
        review.confirmed = true;
        review.selected_activations = choices.filter(item => item.checkbox.checked).map(item => item.activation);
        try { await this.commandHandler({kind: 'finish', partialFinish: review, model: this.#model}); }
        catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
      });
      form.append(confirmation, submit);
      if (partial.capability?.reason) form.append(element('p', 'recovery-notice', partial.capability.reason));
      this.#content.append(form);
    }
  }
  #render() {
    const node = this.#model?.nodes?.find((candidate) => candidate.node_id === this.#nodeId);
    this.hidden = !node && !this.#turnMode;
    if (this.hidden) { this.#dialog.close(); return; }
    const focused = this.#root.activeElement?.className;
    const scroll = this.#dialog.scrollTop;
    const activations = node?.activations || [];
    const selected = this.#followLatest ? activations.at(-1)
      : activations.find((item) => this.#activationKey && activationSelectionKey(item.reference) === this.#activationKey)
        || activations.findLast((item) => !this.#activationKey && evidenceValue(item.generation) === this.#generation);
    if (selected) this.#generation = evidenceValue(selected.generation);
    this.#content.replaceChildren();
    const header = element('header');
    const title = element('div');
    title.append(element('h2', '', node ? node.label || node.node_id : 'Turn controls'), element('p', 'label', this.#model.turnId || 'Configured team · no turn selected'));
    const close = element('button', 'close', 'Close');
    close.type = 'button'; close.addEventListener('click', () => this.close());
    header.append(title, close); this.#content.append(header);
    if (this.#model?.controlPlane?.history_version === 'execution_v2') {const grants=element('button','current-authority','Review current authority');grants.onclick=()=>void this.#grants.open(this.#model);this.#content.append(grants);}
    if (this.#graphEditor.pending(this.#model)) {
      const pendingGraph = element('button', 'resolve-graph-edit', 'Resolve pending graph change');
      pendingGraph.type = 'button'; pendingGraph.onclick = () => this.#graphEditor.resumePending(this.#model);
      this.#content.append(pendingGraph);
    }
    if (this.#model?.controlPlane?.history_version === 'execution_v2'
        && this.#model.controlPlane.state === 'running' && !this.#model.stopRequested) {
      const add = element('button', 'add-current-work', node ? 'Add dependent work' : 'Add work to this turn');
      add.type = 'button'; add.onclick = () => void this.#graphEditor.open(this.#model, node, 'add');
      this.#content.append(add);
      if (node && !activations.some(item => item.state !== 'unstarted')) {
        const replace = element('button', 'replace-current-work', 'Replace before starting');
        replace.type = 'button'; replace.onclick = () => void this.#graphEditor.open(this.#model, node, 'replace');
        this.#content.append(replace);
      }
    }
    if (!node) {
      this.#renderRequiredChecks();
      this.#renderTurnControls();
      this.#renderReceipts(this.#commandHistory);
      this.#syncPresentation(); this.#dialog.scrollTop = scroll;
      return;
    }
    this.#content.append(element('h3', '', 'Definition used for this turn'));
    const definition = evidenceValue(node.definition);
    if (definition) {
      const rows = element('dl');
      for (const [label, key] of [['Name', 'name'], ['Role', 'role'], ['Provider', 'provider'], ['Model', 'model'], ['Configuration', 'configuration_revision']]) {
        this.#row(rows, label, definition[key]);
      }
      this.#content.append(rows);
    } else this.#content.append(element('p', 'unavailable', evidenceText(node.definition)));
    if (this.#model.stopRequested?.unrun_nodes.includes(node.node_id)) {
      this.#content.append(element('p', 'notice', this.#model.stopRequested.partial_finish ? 'Skipped by confirmed partial Finish' : 'Stopped before starting'));
      this.#content.append(element('p', 'label', `${this.#model.stopRequested.partial_finish ? 'Partial Finish' : 'Stop'} command ${this.#model.stopRequested.command_id}`));
    }
    this.#content.append(element('h3', '', 'Selected activation'));
    if (activations.length > 1) {
      const chooser = element('select', 'generation'); chooser.setAttribute('aria-label', 'Activation generation');
      if (!selected) { const unavailable = element('option', '', 'Referenced activation unavailable'); unavailable.value = ''; unavailable.disabled = true; unavailable.selected = true; chooser.append(unavailable); }
      activations.forEach((activation, index) => {
        const generation = evidenceValue(activation.generation);
        const option = element('option', '', `${generation === null ? 'Generation not recorded' : `Generation ${generation}`} · ${activation.state}`);
        option.value = String(index); option.selected = activation === selected; chooser.append(option);
      });
      chooser.addEventListener('change', () => {
        const activation = activations[Number(chooser.value)];
        this.#generation = evidenceValue(activation.generation);
        this.#activationKey = activationSelectionKey(activation.reference); this.#followLatest = false;
        this.dispatchEvent(new CustomEvent('selected-activation-change', {
          detail: { nodeId: node.node_id, generation: this.#generation, reference: activation.reference }, bubbles: true, composed: true,
        }));
        this.#render();
      });
      this.#content.append(chooser);
    }
    if (selected) {
      const commands = this.#commandHistory.filter((command) => {
        const target = selected.reference?.activation;
        return target && activationSelectionKey({ kind: 'exact', activation: command.request?.activation })
          === activationSelectionKey(selected.reference);
      });
      if (typeof this.commandHandler === 'function' && selected.reference?.kind === 'exact') {
        this.#renderHumanResponses(selected, commands);
      }
      const rows = element('dl');
      this.#row(rows, 'State', recorded(selected.state));
      this.#row(rows, 'Generation', selected.generation);
      this.#row(rows, 'Reason', selected.reason);
      for (const [label, key] of [['Started', 'started_at'], ['Finished', 'completed_at']]) {
        const value = evidenceValue(selected[key]);
        this.#row(rows, label, typeof value === 'number' ? recorded(new Date(value).toLocaleString()) : selected[key]);
      }
      this.#content.append(rows);
      this.#block('Input', selected.input);
      this.#block('Output', selected.output);
      const outputEvidence = (selected.evidence || []).find(item => ['agent_output', 'acceptance'].includes(item.kind)
        && evidenceValue(item.reference));
      if (outputEvidence) {
        const addOutput = this.#referenceButton(outputEvidence, selected, node, 'output', 'Add output to chat');
        if (addOutput) this.#content.append(addOutput);
      }
      (selected.partial_outputs || []).forEach((partial) => this.#block('Partial output', {
        status: partial.truncated ? 'truncated' : 'available', value: partial.text,
      }));
      this.#block('Usage', selected.usage);
      this.#content.append(element('h3', '', 'Evidence and causal history'));
      if (!(selected.evidence || []).length) this.#content.append(element('p', 'unavailable', 'Not recorded'));
      for (const item of selected.evidence || []) {
        const evidence = element('div', 'evidence');
        evidence.append(element('strong', '', item.kind.replaceAll('_', ' ')), element('p', '', evidenceText(item.summary)));
        const details = document.createElement('details');
        details.append(element('summary', '', 'Recorded details'), element('pre', '', evidenceText(item.details)));
        evidence.append(details);
        const type = ['agent_output', 'acceptance'].includes(item.kind) ? 'output' : 'event';
        const add = this.#referenceButton(item, selected, node, type);
        if (add) evidence.append(add);
        this.#content.append(evidence);
      }
      this.#renderReceipts(commands);
      // Runtime commands appear only when the server both authorizes the exact
      // activation and the host provides a real command implementation.
      if (typeof this.commandHandler === 'function' && selected.reference?.kind === 'exact') {
        const actions = element('div', 'actions');
        for (const [kind, label] of [['stop', 'Stop'], ['retry', 'Retry']]) {
          if (selected.capabilities?.[kind]?.enabled !== true) continue;
          const button = element('button', kind, label); button.type = 'button';
          button.disabled = commands.some(command => command.request.action === kind
            && !['settled', 'rejected', 'failed'].includes(command.receipt?.state));
          button.addEventListener('click', async () => {
            button.disabled = true;
            try { await this.commandHandler({ kind, reference: selected.reference, model: this.#model }); }
            catch (error) { this.#content.append(element('p', 'notice', error.message || String(error))); }
            finally { button.disabled = false; }
          });
          actions.append(button);
        }
        this.#content.append(actions);
        this.#renderGuidance(selected, commands);
        this.#renderRevision(selected, commands);
      }
    } else this.#content.append(element('p', 'unavailable', this.#followLatest ? 'No activation recorded' : 'The referenced activation is unavailable in this recorded turn.'));
    if (this.coordinatorAgentId) {
      const open = element('button', 'coordinator-details', 'Open Coordinator details'); open.type = 'button';
      open.addEventListener('click', () => this.dispatchEvent(new CustomEvent('open-coordinator-details', {
        detail: { agentId: this.coordinatorAgentId }, bubbles: true, composed: true,
      })));
      this.#content.append(open);
    }
    for (const warning of this.#model.warnings || []) this.#content.append(element('p', 'notice', warning));
    this.#syncPresentation();
    this.#dialog.scrollTop = scroll;
    if (focused) this.#root.querySelector(`.${globalThis.CSS.escape(focused)}`)?.focus();
  }
}

if (!customElements.get('ax-activation-inspector')) customElements.define('ax-activation-inspector', AxActivationInspector);
