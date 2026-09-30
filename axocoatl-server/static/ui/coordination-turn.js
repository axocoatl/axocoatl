import { adopt } from './sheets.js';
import { foldControlPlane } from './activation-inspector.js';

/**
 * `<ax-coordination-turn>` — durable causal evidence for one coordinated turn.
 *
 * The component is deliberately transport-free. Give it either a canonical
 * Session `turn` (whose `execution_events` use the turn-ledger envelope) or an
 * `events` array while a caller is assembling a live projection. The shell owns
 * navigation; this element only asks it to open the existing Agent graph.
 *
 * Coordination event metadata accepts these stable fields:
 *
 * - `coordination_planned`: `agents`, plus optional `dependencies` / `edges`.
 *   Agent entries may be ids or `{id|agent_id, name|label, depends_on}`.
 * - `coordination_signal`: `from_agent`, `to_agent`, `summary`.
 * - agent lifecycle events (including `coordination_agent_reactivated` and
 *   `coordination_agent_cancelled`): `agent_id`, `generation`, optional
 *   `summary` / `reason`, `parents`, and `cause_signal_ids`.
 * - `coordination_completed`: optional `summary` / `output` and `status`.
 *
 * Aliases used by existing runtime projections (`agent`, `worker`, `from`,
 * `to`, `source`, `target`, `message`, `error`) are accepted as a compatibility
 * boundary. Unknown event kinds are ignored.
 *
 * @element ax-coordination-turn
 *
 * @prop {object|null} turn Canonical Session turn.
 * @prop {Array<object>|null} events Coordination execution events. Setting this
 *   after `turn` makes it the active source; set it to `null` to return to the
 *   turn's `execution_events`.
 *
 * @fires open-agent-graph detail: {turnId}
 */

const COORDINATION_KINDS = new Set([
  'coordination_planned',
  'coordination_signal',
  'coordination_agent_activated',
  'coordination_agent_reactivated',
  'coordination_agent_completed',
  'coordination_agent_failed',
  'coordination_agent_blocked',
  'coordination_agent_cancelled',
  'coordination_recovery_partial',
  'coordination_completed',
  'agent_output_superseded',
]);

const KIND_LABELS = {
  coordination_planned: 'Plan',
  coordination_signal: 'Handoff',
  coordination_agent_activated: 'Agent started',
  coordination_agent_reactivated: 'Agent queued again',
  coordination_agent_completed: 'Agent completed',
  coordination_agent_failed: 'Agent failed',
  coordination_agent_blocked: 'Agent blocked',
  coordination_agent_cancelled: 'Agent stopped',
  coordination_recovery_partial: 'Recovery evidence retained',
  coordination_completed: 'Answer completed',
  agent_output_superseded: 'Output superseded',
};

const TERMINAL_TURN_STATES = new Set(['completed', 'failed', 'cancelled', 'interrupted']);
const PSEUDO_AGENTS = new Set(['user', 'request', 'answer', 'result']);

const CSS = `
:host {
  color: var(--text, #ececec); display: block;
  font: var(--fs-body, 13.5px)/var(--lh-body, 1.55) var(--font-sans, ui-sans-serif, sans-serif);
  margin: var(--sp-2, 8px) 0; max-width: 900px;
}
:host([empty]) { display: none; }
* { box-sizing: border-box; }
.card {
  background: var(--panel, #1c1c1e); border: 1px solid var(--border, #2c2c2e);
  border-left: 3px solid var(--accent, #3e7c5c); border-radius: var(--r-lg, 10px);
  box-shadow: var(--shadow-sm, 0 1px 2px rgba(0,0,0,.3)); overflow: hidden;
}
.head {
  align-items: center; display: flex; gap: var(--sp-3, 12px);
  min-height: 46px; padding: var(--sp-2, 8px) var(--sp-3, 12px);
}
.mark {
  align-items: center; background: rgba(var(--axo-jade-rgb, 62, 124, 92), .14);
  border-radius: var(--r-md, 6px); color: var(--accent, #3e7c5c); display: inline-flex;
  flex: 0 0 28px; height: 28px; justify-content: center; width: 28px;
}
.summary { flex: 1; font-size: var(--fs-sm, 12.5px); font-weight: var(--fw-medium, 500); min-width: 0; }
.status { color: var(--muted, #9a9a9a); white-space: nowrap; }
.status::before, .agent-state::before {
  background: currentColor; border-radius: 50%; content: ''; display: inline-block;
  height: 6px; margin-right: var(--sp-1, 4px); vertical-align: 1px; width: 6px;
}
.live-status {
  clip: rect(0 0 0 0); clip-path: inset(50%); height: 1px; overflow: hidden;
  position: absolute; white-space: nowrap; width: 1px;
}
[data-state="working"] { color: var(--axo-blue-glow, #6fd3ee); }
[data-state="completed"] { color: var(--ok, #5bcc8a); }
[data-state="failed"] { color: var(--err, #e26a6a); }
[data-state="blocked"], [data-state="interrupted"], [data-state="stopped"], [data-state="finished"] { color: var(--warn, #e8b25a); }
[data-state="planned"], [data-state="waiting"] { color: var(--muted, #9a9a9a); }
.open-graph {
  background: transparent; border: 1px solid var(--border-strong, #3a3a3c);
  border-radius: var(--r-md, 6px); color: var(--text, #ececec); cursor: pointer;
  flex: 0 0 auto; font: var(--fw-medium, 500) var(--fs-xs, 11px) var(--font-sans, sans-serif);
  padding: 4px var(--sp-2, 8px); transition: border-color var(--dur-fast, 90ms) var(--ease, ease), color var(--dur-fast, 90ms) var(--ease, ease);
}
.open-graph:hover { border-color: var(--accent, #3e7c5c); color: var(--accent, #3e7c5c); }
.open-graph:focus-visible, summary:focus-visible { outline: none; box-shadow: var(--focus-ring, 0 0 0 3px rgba(62,124,92,.35)); }
.causal {
  border-top: 1px solid var(--border, #2c2c2e); padding: var(--sp-3, 12px);
}
.flow {
  align-items: stretch; display: flex; gap: var(--sp-2, 8px); list-style: none;
  margin: 0; padding: 0;
}
.flow-node {
  background: var(--bg-3, #141414); border: 1px solid var(--border, #2c2c2e);
  border-radius: var(--r-md, 6px); color: var(--text, #ececec); min-width: 0;
  padding: var(--sp-2, 8px);
}
.endpoint { flex: 0 1 170px; }
.agent-group { flex: 1 1 320px; list-style: none; min-width: 0; }
.agents {
  display: grid; gap: var(--sp-2, 8px); grid-template-columns: repeat(auto-fit, minmax(145px, 1fr));
  list-style: none; margin: 0; padding: 0;
}
.agent { min-height: 68px; }
.agent[data-state="working"] { border-color: var(--axo-blue, #3fa9c8); }
.agent[data-state="completed"] { border-color: var(--ok, #5bcc8a); }
.agent[data-state="failed"] { border-color: var(--err, #e26a6a); }
.agent[data-state="blocked"], .agent[data-state="interrupted"], .agent[data-state="stopped"] { border-color: var(--warn, #e8b25a); }
.arrow { align-self: center; color: var(--muted-2, #6a6a6a); flex: 0 0 auto; font-family: var(--font-mono, monospace); }
.eyebrow {
  color: var(--muted-2, #6a6a6a); display: block; font-size: var(--fs-xs, 11px);
  letter-spacing: .04em; line-height: var(--lh-tight, 1.25); text-transform: uppercase;
}
.node-title { display: block; font-size: var(--fs-sm, 12.5px); font-weight: var(--fw-medium, 500); margin-top: 2px; overflow-wrap: anywhere; }
.node-copy, .agent-deps, .agent-summary {
  color: var(--muted, #9a9a9a); display: -webkit-box; font-size: var(--fs-xs, 11px);
  line-height: 1.35; margin-top: var(--sp-1, 4px); overflow: hidden; overflow-wrap: anywhere;
  -webkit-box-orient: vertical; -webkit-line-clamp: 2;
}
.answer-outputs { display: grid; gap: var(--sp-2, 8px); margin-top: var(--sp-1, 4px); }
.answer-output { border-top: 1px solid var(--border, #2c2c2e); display: grid; gap: 2px; padding-top: var(--sp-1, 4px); }
.answer-output:first-child { border-top: 0; padding-top: 0; }
.answer-agent { color: var(--accent-2, #3fa9c8); font: var(--fs-xs, 11px) var(--font-mono, monospace); overflow-wrap: anywhere; }
.answer-copy { color: var(--muted, #9a9a9a); font-size: var(--fs-xs, 11px); line-height: 1.35; overflow-wrap: anywhere; }
.recovery-evidence { border-top: 1px solid var(--border, #2c2c2e); display: grid; gap: 3px; margin-top: var(--sp-1, 4px); padding-top: var(--sp-1, 4px); }
.recovery-label { color: var(--warn, #e8b25a); font-size: var(--fs-xs, 11px); font-weight: var(--fw-medium, 500); }
.recovery-copy { color: var(--muted, #9a9a9a); font-size: var(--fs-xs, 11px); line-height: 1.35; max-height: 8em; overflow: auto; overflow-wrap: anywhere; white-space: pre-wrap; }
.agent-head { align-items: baseline; display: flex; gap: var(--sp-2, 8px); justify-content: space-between; }
.agent-name { font-size: var(--fs-sm, 12.5px); font-weight: var(--fw-medium, 500); min-width: 0; overflow-wrap: anywhere; }
.agent-state { flex: 0 0 auto; font-size: var(--fs-xs, 11px); white-space: nowrap; }
.handoffs { margin-top: var(--sp-3, 12px); }
.section-title {
  color: var(--muted-2, #6a6a6a); font-size: var(--fs-xs, 11px); font-weight: var(--fw-medium, 500);
  letter-spacing: .04em; margin: 0 0 var(--sp-1, 4px); text-transform: uppercase;
}
.handoff-list, .timeline { list-style: none; margin: 0; padding: 0; }
.handoff {
  align-items: baseline; border-top: 1px solid var(--border, #2c2c2e); display: grid;
  gap: var(--sp-2, 8px); grid-template-columns: minmax(100px, auto) minmax(0, 1fr);
  padding: 5px 0;
}
.handoff:first-child { border-top: 0; }
.handoff-route { color: var(--accent-2, #3fa9c8); font: var(--fs-xs, 11px) var(--font-mono, monospace); overflow-wrap: anywhere; }
.handoff-summary { color: var(--muted, #9a9a9a); font-size: var(--fs-xs, 11px); overflow-wrap: anywhere; }
details { border-top: 1px solid var(--border, #2c2c2e); }
summary {
  color: var(--muted, #9a9a9a); cursor: pointer; font-size: var(--fs-xs, 11px);
  list-style-position: inside; padding: var(--sp-2, 8px) var(--sp-3, 12px);
}
summary:hover { color: var(--text, #ececec); }
.timeline { border-top: 1px solid var(--border, #2c2c2e); padding: var(--sp-1, 4px) var(--sp-3, 12px) var(--sp-2, 8px); }
.event {
  align-items: baseline; display: grid; gap: var(--sp-2, 8px);
  grid-template-columns: minmax(110px, auto) minmax(0, 1fr); padding: 5px 0;
}
.event + .event { border-top: 1px solid var(--border, #2c2c2e); }
.event-label { font-size: var(--fs-xs, 11px); font-weight: var(--fw-medium, 500); }
.event-summary { color: var(--muted, #9a9a9a); font-size: var(--fs-xs, 11px); overflow-wrap: anywhere; }
@media (max-width: 560px) {
  .head { align-items: flex-start; flex-wrap: wrap; }
  .summary { flex-basis: calc(100% - 44px); }
  .open-graph { margin-left: 40px; }
  .flow { flex-direction: column; }
  .endpoint, .agent-group { flex-basis: auto; width: 100%; }
  .arrow { align-self: flex-start; margin-left: var(--sp-3, 12px); transform: rotate(90deg); }
  .handoff, .event { grid-template-columns: 1fr; gap: 1px; }
}
@media (prefers-reduced-motion: reduce) {
  .open-graph { transition: none; }
}
`;

export function isObject(value) {
  return value != null && typeof value === 'object' && !Array.isArray(value);
}

export function firstText(...values) {
  for (const value of values) {
    if (typeof value === 'string' && value.trim()) return value.trim();
    if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  }
  return '';
}

function asList(value) {
  if (Array.isArray(value)) return value;
  if (typeof value === 'string' && value.trim()) return [value.trim()];
  return [];
}

export function short(value, length = 180) {
  const text = firstText(value).replace(/\s+/g, ' ');
  return text.length > length ? `${text.slice(0, Math.max(0, length - 1)).trimEnd()}…` : text;
}

function metadataOf(entry) {
  const body = isObject(entry?.event) ? entry.event : entry;
  const outer = isObject(entry?.metadata) ? entry.metadata : {};
  const inner = isObject(body?.metadata) ? body.metadata : {};
  return { ...outer, ...inner };
}

export function normalizeEvent(entry, index) {
  if (!isObject(entry)) return null;
  const body = isObject(entry.event) ? entry.event : entry;
  const kind = firstText(body.kind, entry.kind);
  if (!COORDINATION_KINDS.has(kind)) return null;
  return {
    kind,
    metadata: metadataOf(entry),
    operationId: firstText(entry.operation_id, body.operation_id, entry.id, body.id, String(index)),
    recordedOperationId: firstText(entry.operation_id, body.operation_id),
    recordedAt: Number(entry.recorded_at ?? body.recorded_at ?? entry.timestamp ?? body.timestamp ?? index),
    turnId: firstText(entry.turn_id, body.turn_id, metadataOf(entry).turn_id),
  };
}

export function normalizedAgentState(value, fallback = 'waiting') {
  switch (String(value || '').toLowerCase()) {
    case 'active':
    case 'activated':
    case 'running':
    case 'working': return 'working';
    case 'accepted':
    case 'complete':
    case 'completed':
    case 'done':
    case 'success': return 'completed';
    case 'error':
    case 'failed': return 'failed';
    case 'blocked':
    case 'waiting_for_input': return 'blocked';
    case 'cancelled':
    case 'canceled':
    case 'stopped': return 'stopped';
    case 'unstarted':
    case 'planned':
    case 'queued':
    case 'pending':
    case 'waiting': return 'waiting';
    default: return fallback;
  }
}

export function normalizedRunState(value, fallback = '') {
  switch (String(value || '').toLowerCase()) {
    case 'complete':
    case 'completed':
    case 'done':
    case 'success': return 'completed';
    case 'active':
    case 'running':
    case 'working':
    case 'synthesizing': return 'working';
    case 'blocked':
    case 'waiting_for_input': return 'blocked';
    case 'error':
    case 'failed': return 'failed';
    case 'cancelled':
    case 'canceled':
    case 'stopped': return 'stopped';
    case 'interrupted': return 'interrupted';
    case 'planned':
    case 'queued':
    case 'pending': return 'planned';
    default: return fallback;
  }
}

export function eventSummary(metadata, kind) {
  const common = [
    metadata.summary, metadata.handoff_summary, metadata.message,
    metadata.reason, metadata.blocker, metadata.error,
  ];
  if (kind === 'coordination_planned') {
    return firstText(metadata.goal, metadata.request, metadata.user_input, metadata.description, ...common);
  }
  if (kind === 'coordination_agent_activated') {
    return firstText(metadata.input_summary, metadata.activation_reason, ...common);
  }
  if (kind === 'coordination_completed') {
    return firstText(metadata.final_output, metadata.output, metadata.result, ...common);
  }
  if (kind === 'coordination_recovery_partial') {
    const byteLength = Number(metadata.byte_len) || 0;
    return byteLength
      ? `${byteLength} bytes retained from the interrupted turn as unattributed recovery evidence.`
      : 'Interrupted turn output retained as unattributed recovery evidence.';
  }
  if (kind === 'agent_output_superseded') {
    const prior = Number(metadata.activation_generation) || 0;
    const next = Number(metadata.superseded_by_generation) || 0;
    if (prior && next) return `Generation ${prior} replaced by generation ${next}.`;
  }
  return firstText(...common, metadata.output, metadata.result, metadata.description, metadata.task);
}

export function eventAgent(metadata) {
  return firstText(metadata.agent_id, metadata.agent, metadata.worker_id, metadata.worker);
}

function eventFrom(metadata) {
  return firstText(
    metadata.from_agent, metadata.from_agent_id, metadata.from,
    metadata.source_agent, metadata.source_agent_id, metadata.source,
    metadata.produced_by,
  );
}

function eventTo(metadata) {
  return firstText(
    metadata.to_agent, metadata.to_agent_id, metadata.to,
    metadata.target_agent, metadata.target_agent_id, metadata.target,
    metadata.consumer,
  );
}

function dependencyIds(value) {
  return asList(value).map((dependency) => (
    typeof dependency === 'string' ? dependency : firstText(dependency?.id, dependency?.agent_id, dependency?.agent)
  )).filter(Boolean);
}

function topologicalAgents(agents) {
  const ids = agents.map((agent) => agent.id);
  const positions = new Map(ids.map((id, index) => [id, index]));
  const known = new Set(ids);
  const indegree = new Map(ids.map((id) => [id, 0]));
  const outgoing = new Map(ids.map((id) => [id, []]));
  for (const agent of agents) {
    for (const dependency of agent.dependsOn) {
      if (!known.has(dependency) || dependency === agent.id) continue;
      indegree.set(agent.id, (indegree.get(agent.id) || 0) + 1);
      outgoing.get(dependency).push(agent.id);
    }
  }
  const ready = ids.filter((id) => indegree.get(id) === 0);
  const ordered = [];
  while (ready.length) {
    ready.sort((left, right) => positions.get(left) - positions.get(right));
    const id = ready.shift();
    ordered.push(id);
    for (const target of outgoing.get(id) || []) {
      indegree.set(target, indegree.get(target) - 1);
      if (indegree.get(target) === 0) ready.push(target);
    }
  }
  for (const id of ids) if (!ordered.includes(id)) ordered.push(id);
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  return ordered.map((id) => byId.get(id));
}

/**
 * Fold durable or live coordination events into one renderable turn model.
 * Exported so protocol-focused tests can verify the contract without reaching
 * through the custom element's shadow root.
 */
export function foldCoordinationEvents(events = [], turn = null) {
  const source = Array.isArray(events) ? events : [];
  const normalized = [];
  const seen = new Set();
  source.forEach((entry, index) => {
    const event = normalizeEvent(entry, index);
    if (!event) return;
    const identity = event.operationId || `${event.kind}:${index}`;
    if (seen.has(identity)) return;
    seen.add(identity);
    normalized.push(event);
  });

  const agents = new Map();
  const handoffs = [];
  const timeline = [];
  let declaredAgentCount = 0;
  let request = firstText(turn?.user_input, turn?.display_input);
  const turnAnswer = firstText(turn?.final_output, turn?.partial_output);
  let coordinationAnswer = '';
  let answer = '';
  let explicitStatus = '';
  let planned = false;
  const declaredSinks = [];

  const registerAgent = (value, options = {}) => {
    const id = typeof value === 'string'
      ? firstText(value)
      : firstText(value?.id, value?.agent_id, value?.agent, value?.worker_id, value?.worker);
    if (!id || PSEUDO_AGENTS.has(id.toLowerCase())) return null;
    let agent = agents.get(id);
    if (!agent) {
      agent = { id, label: id, state: 'waiting', dependsOn: new Set(), summary: '', lastKind: '' };
      agents.set(id, agent);
    }
    const record = isObject(value) ? value : {};
    agent.label = firstText(options.label, record.label, record.name, record.display_name, agent.label);
    const dependencies = [
      ...dependencyIds(options.dependsOn),
      ...dependencyIds(record.depends_on),
      ...dependencyIds(record.dependencies),
      ...dependencyIds(record.caused_by),
    ];
    dependencies.forEach((dependency) => {
      if (dependency !== id && !PSEUDO_AGENTS.has(dependency.toLowerCase())) agent.dependsOn.add(dependency);
    });
    if (options.state) agent.state = normalizedAgentState(options.state, agent.state);
    if (options.summary) agent.summary = short(options.summary);
    if (options.kind) agent.lastKind = options.kind;
    return agent;
  };

  const registerPlanSource = (plan) => {
    if (!isObject(plan)) return;
    const lists = [plan.agents, plan.participants, plan.roster, plan.agent_ids];
    for (const list of lists) {
      if (Array.isArray(list)) list.forEach((agent) => registerAgent(agent));
      else if (isObject(list)) {
        Object.entries(list).forEach(([id, value]) => registerAgent(
          isObject(value) ? { id, ...value } : id,
        ));
      }
    }
    for (const assignment of asList(plan.assignments)) {
      if (isObject(assignment)) registerAgent(assignment);
    }
    const edgeSources = [plan.dependencies, plan.edges];
    for (const edgeSource of edgeSources) {
      if (isObject(edgeSource) && !Array.isArray(edgeSource)) {
        Object.entries(edgeSource).forEach(([target, dependencies]) => {
          const agent = registerAgent(target);
          dependencyIds(dependencies).forEach((dependency) => {
            registerAgent(dependency);
            if (agent && dependency !== agent.id) agent.dependsOn.add(dependency);
          });
        });
        continue;
      }
      for (const edge of asList(edgeSource)) {
        const from = Array.isArray(edge) ? firstText(edge[0]) : firstText(edge?.from_agent, edge?.from, edge?.source);
        const to = Array.isArray(edge) ? firstText(edge[1]) : firstText(edge?.to_agent, edge?.to, edge?.target);
        if (!from || !to || from === to) continue;
        const target = registerAgent(to);
        registerAgent(from);
        target?.dependsOn.add(from);
      }
    }
    declaredAgentCount = Math.max(
      declaredAgentCount,
      Number(plan.agent_count || plan.agentCount || 0) || 0,
    );
    for (const sink of asList(plan.sinks)) {
      const id = typeof sink === 'string'
        ? firstText(sink)
        : firstText(sink?.id, sink?.agent_id, sink?.agent);
      if (!id || PSEUDO_AGENTS.has(id.toLowerCase()) || declaredSinks.includes(id)) continue;
      declaredSinks.push(id);
      registerAgent(id);
    }
  };

  normalized.forEach((event) => {
    const metadata = event.metadata;
    const summary = eventSummary(metadata, event.kind);
    const agentId = eventAgent(metadata);
    let from = eventFrom(metadata);
    let to = eventTo(metadata);
    let state = 'planned';

    if (event.kind === 'coordination_planned') {
      planned = true;
      registerPlanSource(metadata);
      registerPlanSource(metadata.plan);
      request = firstText(metadata.request, metadata.user_input, metadata.goal, request);
      declaredAgentCount = Math.max(declaredAgentCount, agents.size);
    } else if (event.kind === 'coordination_signal') {
      // Signals explain a causal handoff, but they do not rewrite the immutable
      // dependency graph snapshotted by coordination_planned. In particular, a
      // reviewer can request changes from an upstream builder without turning
      // the feedback edge into a dependency cycle in this projection.
      registerAgent(from);
      registerAgent(to);
      if (summary || from || to) {
        handoffs.push({ from: from || 'Request', to: to || 'Answer', summary: short(summary || 'Signal ready'), kind: event.kind });
      }
      state = 'working';
    } else if (event.kind === 'coordination_agent_activated') {
      const agent = registerAgent(agentId, {
        dependsOn: metadata.parents || metadata.caused_by || metadata.depends_on || metadata.waiting_for,
        state: 'working', summary, kind: event.kind,
      });
      if (agent) {
        from = from || [...agent.dependsOn].join(', ') || 'Request';
        to = agent.id;
      }
      state = 'working';
    } else if (event.kind === 'coordination_agent_reactivated') {
      registerAgent(agentId, {
        dependsOn: metadata.parents || metadata.depends_on,
        state: 'waiting', summary, kind: event.kind,
      });
      state = 'waiting';
    } else if (event.kind === 'agent_output_superseded') {
      registerAgent(agentId, { state: 'waiting', summary, kind: event.kind });
      state = 'waiting';
    } else if (event.kind === 'coordination_agent_completed') {
      const agent = registerAgent(agentId, { state: 'completed', summary, kind: event.kind });
      if (agent && summary) {
        const dependents = [...agents.values()]
          .filter((candidate) => candidate.dependsOn.has(agent.id))
          .map((candidate) => candidate.id);
        const destinations = to ? [to] : (dependents.length ? dependents : ['Answer']);
        destinations.forEach((destination) => handoffs.push({
          from: agent.id,
          to: destination,
          summary: short(summary),
          kind: event.kind,
        }));
      }
      state = 'completed';
    } else if (event.kind === 'coordination_agent_failed') {
      registerAgent(agentId, { state: 'failed', summary, kind: event.kind });
      state = 'failed';
    } else if (event.kind === 'coordination_agent_blocked') {
      registerAgent(agentId, {
        dependsOn: metadata.waiting_for || metadata.depends_on,
        state: 'blocked', summary, kind: event.kind,
      });
      state = 'blocked';
    } else if (event.kind === 'coordination_agent_cancelled') {
      registerAgent(agentId, { state: 'stopped', summary, kind: event.kind });
      state = 'stopped';
    } else if (event.kind === 'coordination_recovery_partial') {
      state = 'interrupted';
    } else if (event.kind === 'coordination_completed') {
      registerPlanSource(metadata);
      coordinationAnswer = firstText(
        metadata.final_output, metadata.output, metadata.result, metadata.summary,
        coordinationAnswer,
      );
      answer = coordinationAnswer;
      explicitStatus = normalizedRunState(metadata.status, 'completed');
      state = explicitStatus;
    }

    const subject = agentId
      ? (agents.get(agentId)?.label || agentId)
      : (from && to ? `${from} → ${to}` : 'Coordination');
    timeline.push({
      kind: event.kind,
      label: event.kind === 'coordination_planned'
        ? KIND_LABELS[event.kind]
        : `${subject} · ${KIND_LABELS[event.kind]}`,
      summary: short(summary || (event.kind === 'coordination_planned'
        ? request : event.kind === 'coordination_completed' ? coordinationAnswer : '')),
      state,
      recordedAt: event.recordedAt,
    });
  });

  let answers = [];
  if (planned) {
    // A coordinated turn's aggregate partial/final fields are transport
    // projections: while several Agents or revisions stream, they can contain
    // concatenated generations. History must use the identity-bearing Agent
    // output ledger instead. The execution timeline above remains intact as
    // evidence that an earlier generation existed and was superseded.
    const currentAgentOutputs = (Array.isArray(turn?.agent_outputs) ? turn.agent_outputs : [])
      .filter((output) => output?.superseded !== true);
    const sinkIds = declaredSinks.length
      ? declaredSinks
      : topologicalAgents([...agents.values()])
        .filter((candidate) => ![...agents.values()]
          .some((dependent) => dependent.dependsOn.has(candidate.id)))
        .map((candidate) => candidate.id);
    const currentBySink = new Map();
    currentAgentOutputs.forEach((output, index) => {
      const agentId = firstText(output?.agent_id, output?.agent);
      if (!sinkIds.includes(agentId)) return;
      const disposition = firstText(output?.disposition).toLowerCase();
      if (disposition && disposition !== 'completed') return;
      const generation = Number(output?.activation_generation) || 0;
      const recordedAt = Number(output?.recorded_at) || 0;
      const prior = currentBySink.get(agentId);
      if (!prior
          || generation > prior.generation
          || (generation === prior.generation && recordedAt > prior.recordedAt)
          || (generation === prior.generation && recordedAt === prior.recordedAt
            && index > prior.index)) {
        currentBySink.set(agentId, { output, generation, recordedAt, index });
      }
    });
    answers = sinkIds.flatMap((agentId) => {
      const current = currentBySink.get(agentId);
      const content = firstText(current?.output?.output, current?.output?.content);
      if (!current || !content) return [];
      return [{
        agentId,
        label: agents.get(agentId)?.label || agentId,
        content,
        generation: current.generation || null,
      }];
    });
    answer = answers.length
      ? answers.map((output) => `${output.label}: ${output.content}`).join('\n\n')
      : coordinationAnswer;
  } else {
    answer = turnAnswer;
  }

  const turnStatus = normalizedRunState(turn?.status);
  const agentValues = [...agents.values()];
  const canonicalTurnIsTerminal = TERMINAL_TURN_STATES.has(String(turn?.status || '').toLowerCase());
  const interruptedRecovery = planned && canonicalTurnIsTerminal && turnStatus === 'interrupted'
    ? firstText(turn?.partial_output) : '';
  if (canonicalTurnIsTerminal && turnStatus === 'interrupted') {
    // A daemon restart is a hard ownership boundary. Without a later durable
    // lifecycle event, nodes that were live or merely waiting at the cut must
    // not continue to look executable in recovered History.
    agentValues.forEach((agent) => {
      if (agent.state === 'working' || agent.state === 'waiting') agent.state = 'interrupted';
    });
  }
  let status = canonicalTurnIsTerminal ? turnStatus : explicitStatus;
  if (!status && agentValues.some((agent) => agent.state === 'working')) status = 'working';
  if (!status && agentValues.some((agent) => agent.state === 'blocked')) status = 'blocked';
  if (!status && agentValues.some((agent) => agent.state === 'failed')) status = 'failed';
  if (!status && agentValues.some((agent) => agent.state === 'stopped')) status = 'stopped';
  if (!status && planned) status = 'planned';
  if (!status && normalized.length) status = turnStatus || 'working';

  const turnId = firstText(turn?.id, normalized.find((event) => event.turnId)?.turnId);
  const projectedAgents = topologicalAgents(agentValues).map((agent) => ({
    ...agent,
    dependsOn: [...agent.dependsOn].filter((dependency) => agents.has(dependency)),
  }));

  return {
    turnId,
    request,
    answer,
    answers,
    status,
    agents: projectedAgents,
    agentCount: Math.max(projectedAgents.length, declaredAgentCount),
    handoffs,
    timeline,
    recoveryEvidence: interruptedRecovery,
    hasCoordination: normalized.length > 0,
  };
}

export function element(tag, className = '', text = '') {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function displayState(state) {
  return state === 'completed' ? 'complete' : state || 'waiting';
}

export class AxCoordinationTurn extends HTMLElement {
  #root;
  #content;
  #liveStatus;
  #turn = null;
  #events = null;
  #controlPlane = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
    this.#liveStatus = element('span', 'live-status');
    this.#liveStatus.setAttribute('role', 'status');
    this.#liveStatus.setAttribute('aria-live', 'polite');
    this.#liveStatus.setAttribute('aria-atomic', 'true');
    this.#content = element('div', 'content');
    this.#root.append(this.#liveStatus, this.#content);
  }

  get controlPlane() { return this.#controlPlane; }
  set controlPlane(value) { this.#controlPlane = value; this.#render(); }

  get turn() { return this.#turn; }
  set turn(value) {
    this.#turn = isObject(value) ? value : null;
    this.#controlPlane = null;
    this.#events = null;
    this.#render();
  }

  get events() { return this.#events; }
  set events(value) {
    this.#events = value == null ? null : (Array.isArray(value) ? value : []);
    this.#render();
  }

  connectedCallback() { this.#render(); }

  #model() {
    const events = this.#events ?? this.#turn?.execution_events ?? [];
    return foldControlPlane(this.#controlPlane ?? this.#turn, events);
  }

  #render() {
    const active = this.#root.activeElement;
    const restoreFocus = active?.classList?.contains('open-graph')
      ? '.open-graph' : active?.tagName === 'SUMMARY' ? 'summary' : '';
    const detailsWasOpen = Boolean(this.#root.querySelector('details')?.open);
    const model = this.#model();
    this.toggleAttribute('empty', !model.hasCoordination && !model.unsupported);
    this.#content.replaceChildren();
    if (model.unsupported) {
      const notice = element('p', 'head', model.warnings[0]);
      notice.setAttribute('role', 'status');
      this.#content.append(notice);
      this.#liveStatus.textContent = '';
      return;
    }
    if (!model.hasCoordination) {
      this.#liveStatus.textContent = '';
      return;
    }

    const card = element('article', 'card');
    card.dataset.state = model.status;
    card.setAttribute('aria-label', `Coordination for turn ${model.turnId || 'unknown'}`);

    const head = element('header', 'head');
    const mark = element('span', 'mark', '⌬');
    mark.setAttribute('aria-hidden', 'true');
    const summary = element(
      'div',
      'summary',
      `Coordination · ${model.agentCount} agent${model.agentCount === 1 ? '' : 's'} · `,
    );
    const status = element('span', 'status', displayState(model.status));
    status.dataset.state = model.status;
    summary.append(status);
    const liveStatus = `Coordination · ${model.agentCount} agent${model.agentCount === 1 ? '' : 's'} · ${displayState(model.status)}`;
    this.#liveStatus.dataset.state = model.status;
    if (this.#liveStatus.textContent !== liveStatus) this.#liveStatus.textContent = liveStatus;
    const open = element('button', 'open-graph', 'Open Agent graph');
    open.type = 'button';
    open.addEventListener('click', () => {
      this.dispatchEvent(new CustomEvent('open-agent-graph', {
        detail: { turnId: model.turnId }, bubbles: true, composed: true,
      }));
    });
    head.append(mark, summary, open);
    card.append(head);

    const causal = element('section', 'causal');
    causal.setAttribute('aria-label', 'Coordination route');
    const flow = element('ol', 'flow');
    flow.append(this.#endpoint('request', 'User request', model.request || 'This turn'));
    flow.append(this.#arrow());

    const agentGroup = element('li', 'agent-group');
    agentGroup.dataset.kind = 'agents';
    const agentList = element('ol', 'agents');
    agentList.setAttribute('aria-label', `${model.agentCount} coordinating agent${model.agentCount === 1 ? '' : 's'}`);
    if (model.agents.length) {
      model.agents.forEach((agent) => agentList.append(this.#agent(agent)));
    } else {
      const unknown = element('li', 'flow-node agent');
      unknown.dataset.state = 'waiting';
      unknown.append(
        element('span', 'eyebrow', 'Team'),
        element('span', 'node-title', `${model.agentCount} agent${model.agentCount === 1 ? '' : 's'} planned`),
      );
      agentList.append(unknown);
    }
    agentGroup.append(agentList);
    flow.append(agentGroup);
    flow.append(this.#arrow());
    const answerState = model.status === 'completed' ? 'completed'
      : ['failed', 'blocked', 'interrupted', 'stopped', 'finished'].includes(model.status) ? model.status : 'waiting';
    flow.append(this.#answerEndpoint(
      model.answers,
      model.answer || (answerState === 'waiting'
        ? 'Waiting for coordination' : displayState(answerState)),
      answerState,
      model.recoveryEvidence,
    ));
    causal.append(flow);

    if (model.handoffs.length) causal.append(this.#handoffs(model.handoffs));
    card.append(causal);
    card.append(this.#details(model.timeline, detailsWasOpen));
    this.#content.append(card);
    if (restoreFocus) this.#root.querySelector(restoreFocus)?.focus?.();
  }

  #endpoint(kind, title, copy, state = '') {
    const node = element('li', 'flow-node endpoint');
    node.dataset.kind = kind;
    if (state) node.dataset.state = state;
    node.append(element('span', 'eyebrow', kind === 'request' ? 'Start' : 'Finish'));
    node.append(element('strong', 'node-title', title));
    const description = element('span', 'node-copy', short(copy, 120));
    if (copy && description.textContent !== copy) description.title = copy;
    node.append(description);
    return node;
  }

  #answerEndpoint(outputs, fallback, state, recoveryEvidence = '') {
    const hasOutputs = Array.isArray(outputs) && outputs.length > 0;
    if (!hasOutputs && !recoveryEvidence) {
      return this.#endpoint('answer', 'Answer', fallback, state);
    }
    const node = element('li', 'flow-node endpoint');
    node.dataset.kind = hasOutputs ? 'answer' : 'recovery';
    node.dataset.state = state;
    node.append(element('span', 'eyebrow', 'Finish'));
    node.append(element(
      'strong', 'node-title',
      hasOutputs ? (outputs.length === 1 ? 'Answer' : 'Answers') : 'Recovery evidence',
    ));
    if (hasOutputs) {
      const list = element('span', 'answer-outputs');
      outputs.forEach((output) => {
        const item = element('span', 'answer-output');
        item.dataset.agentId = output.agentId;
        if (output.generation) item.dataset.generation = String(output.generation);
        item.append(element('span', 'answer-agent', output.label));
        const copy = element('span', 'answer-copy', short(output.content, 120));
        if (copy.textContent !== output.content) copy.title = output.content;
        item.append(copy);
        list.append(item);
      });
      node.append(list);
    }
    if (recoveryEvidence) {
      const recovery = element('span', 'recovery-evidence');
      recovery.append(
        element('span', 'recovery-label', 'Unattributed recovery evidence'),
        element('span', 'recovery-copy', recoveryEvidence),
      );
      node.append(recovery);
    }
    return node;
  }

  #arrow() {
    const arrow = element('li', 'arrow', '→');
    arrow.setAttribute('aria-hidden', 'true');
    arrow.setAttribute('role', 'presentation');
    return arrow;
  }

  #agent(agent) {
    const node = element('li', 'flow-node agent');
    node.dataset.agentId = agent.id;
    node.dataset.state = agent.state;
    const dependencyText = agent.dependsOn.length
      ? `after ${agent.dependsOn.join(', ')}` : 'from the request';
    node.setAttribute('aria-label', `${agent.label}, ${displayState(agent.state)}, ${dependencyText}`);
    const head = element('div', 'agent-head');
    head.append(element('span', 'agent-name', agent.label));
    const state = element('span', 'agent-state', displayState(agent.state));
    state.dataset.state = agent.state;
    head.append(state);
    node.append(head, element('span', 'agent-deps', dependencyText));
    if (agent.summary) {
      const summary = element('span', 'agent-summary', agent.summary);
      summary.title = agent.summary;
      node.append(summary);
    }
    return node;
  }

  #handoffs(handoffs) {
    const section = element('section', 'handoffs');
    const title = element('h3', 'section-title', 'Latest handoffs');
    const list = element('ul', 'handoff-list');
    handoffs.slice(-3).reverse().forEach((handoff) => {
      const item = element('li', 'handoff');
      item.append(
        element('span', 'handoff-route', `${handoff.from} → ${handoff.to}`),
        element('span', 'handoff-summary', handoff.summary || 'Signal ready'),
      );
      list.append(item);
    });
    section.append(title, list);
    return section;
  }

  #details(timeline, open) {
    const details = document.createElement('details');
    details.open = open;
    const summary = element('summary', '', `Details · ${timeline.length} event${timeline.length === 1 ? '' : 's'}`);
    const list = element('ol', 'timeline');
    timeline.forEach((event) => {
      const item = element('li', 'event');
      item.dataset.kind = event.kind;
      item.dataset.state = event.state;
      item.append(
        element('span', 'event-label', event.label),
        element('span', 'event-summary', event.summary || displayState(event.state)),
      );
      list.append(item);
    });
    details.append(summary, list);
    return details;
  }
}

if (!customElements.get('ax-coordination-turn')) {
  customElements.define('ax-coordination-turn', AxCoordinationTurn);
}
