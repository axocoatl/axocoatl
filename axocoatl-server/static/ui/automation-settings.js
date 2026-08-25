import { adopt } from './sheets.js';

/**
 * `<ax-automation-settings>` owns the complete Automation settings surface.
 *
 * The explorer creates canonical records, organizes them into folders, and
 * opens the same graph editor used for existing Automations. A new record
 * starts as a valid TextInput → Agent DAG instead of an empty graph.
 *
 * @element ax-automation-settings
 *
 * @fires automations-change detail: {automations, folders}
 * @fires interrupts-change  detail: {items, count}
 * @fires notify             detail: {title, body, kind}
 * @fires run-started        detail: {automationId, response}
 */

const h = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text != null) node.textContent = text;
  return node;
};

async function jsonRequest(url, options) {
  const response = await fetch(url, options);
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  if (!response.ok) {
    const message = body && typeof body === 'object' ? body.error : body;
    throw new Error(message || `HTTP ${response.status}`);
  }
  return body;
}

const clone = (value) => value == null ? value : JSON.parse(JSON.stringify(value));

const automationIdFromName = (value) => String(value || '')
  .normalize('NFKD')
  .replace(/[\u0300-\u036f]/g, '')
  .toLowerCase()
  .replace(/[^a-z0-9]+/g, '-')
  .replace(/^-+|-+$/g, '')
  .slice(0, 80);

const buildStarterAutomation = ({ id, name, description, agentId, folder, triggerKind, cadence, eventName, skillId, automaticInput }) => {
  const instruction = String(automaticInput || '').trim();
  const trigger = triggerKind === 'schedule'
    ? { kind: 'schedule', every: cadence, input: instruction }
    : triggerKind === 'on_event'
      ? { kind: 'on_event', event: eventName, input: null }
      : triggerKind === 'on_skill'
        ? { kind: 'on_skill', skill_id: skillId }
        : { kind: 'manual' };
  const eventDriven = triggerKind === 'on_event' || triggerKind === 'on_skill';
  return {
    id,
    name,
    description: description || null,
    trigger,
    enabled: true,
    folder: folder || null,
    nodes: [
      {
        id: 'input',
        kind: {
          type: 'text_input',
          label: triggerKind === 'manual' ? 'Prompt' : 'Instruction',
          default_value: triggerKind === 'manual' ? null : instruction,
          placeholder: triggerKind === 'manual' ? 'Describe what this Automation should do…' : null,
          multiline: true,
        },
        position: { x: 0, y: 0 },
      },
      {
        id: 'agent',
        kind: {
          type: 'agent',
          agent_id: agentId,
          input: eventDriven
            ? { kind: 'template', template: '{{node:input}}\n\nTrigger data:\n{{trigger}}' }
            : { kind: 'from_upstream', nodes: ['input'] },
        },
        position: { x: 260, y: 0 },
      },
    ],
    edges: [{ from: 'input', to: 'agent', label: null }],
  };
};

const relativeTime = (seconds) => {
  if (!seconds) return '—';
  const delta = Math.floor(Date.now() / 1000) - Number(seconds);
  if (delta < 60) return 'just now';
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  if (delta < 86400 * 7) return `${Math.floor(delta / 86400)}d ago`;
  return new Date(Number(seconds) * 1000).toLocaleDateString();
};

const CSS = `
:host {
  display: block; min-width: 0; min-height: 0; height: 100%; color: var(--text, #e8ecf3);
  background: var(--panel, #10131a); font-family: var(--font-sans, Inter, system-ui, sans-serif);
}
*, *::before, *::after { box-sizing: border-box; }
button, input, select, textarea { font: inherit; color: inherit; }
button { cursor: pointer; }
.hide { display: none !important; }
.grow { flex: 1; }
.muted { color: var(--muted, #8c94a6); }
.small { font-size: var(--fs-xs, 11px); }
.mono { font-family: var(--font-mono, ui-monospace, monospace); }
.shell { position: relative; height: 100%; min-height: 0; overflow: hidden; }
.errors { position: absolute; top: 8px; left: 50%; transform: translateX(-50%); z-index: 90;
  width: min(720px, calc(100% - 24px)); display: grid; gap: 6px; pointer-events: none; }
.error { display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: center; gap: 8px;
  padding: 9px 11px; color: var(--err, #ff6b6b); background: var(--panel-2, #161b25);
  border: 1px solid var(--err, #ff6b6b); border-radius: var(--r-md, 7px); box-shadow: var(--shadow-md);
  font-size: var(--fs-xs, 11px); pointer-events: auto; }
.error .message { white-space: pre-wrap; overflow-wrap: anywhere; }
.error button { border: 0; background: none; color: inherit; padding: 2px 5px; }
.loading { opacity: .66; }
.btn { min-height: 30px; padding: 5px 11px; border: 1px solid var(--border-strong, #384153);
  border-radius: var(--r-md, 7px); background: var(--accent, #31d6a6); color: var(--bg, #07100d);
  font-size: var(--fs-xs, 11px); font-weight: 600; }
.btn.ghost { background: transparent; color: var(--text, #e8ecf3); }
.btn.danger { background: var(--err, #ff6b6b); border-color: var(--err, #ff6b6b); color: #160707; }
.btn:hover:not(:disabled) { filter: brightness(1.09); border-color: var(--accent, #31d6a6); }
.btn:disabled { cursor: not-allowed; opacity: .45; }
.icon-btn { width: 28px; height: 28px; padding: 0; border: 0; background: none; color: var(--muted, #8c94a6); font-size: 18px; }
.input, .select { width: 100%; min-width: 0; padding: 7px 9px; color: var(--text, #e8ecf3);
  background: var(--bg-2, #0c1017); border: 1px solid var(--border, #293140); border-radius: var(--r-sm, 5px); }
textarea.input { resize: vertical; min-height: 78px; font-family: var(--font-mono, ui-monospace, monospace); line-height: 1.45; }
button:focus-visible, input:focus-visible, select:focus-visible, textarea:focus-visible {
  outline: none; box-shadow: var(--focus-ring, 0 0 0 2px #31d6a6); }

.explorer { display: grid; grid-template-columns: 250px minmax(0, 1fr); height: 100%; min-height: 0; }
.tree { min-height: 0; overflow: hidden; display: flex; flex-direction: column; background: var(--bg-2, #0c1017);
  border-right: 1px solid var(--border, #293140); }
.tree-head, .main-head { flex-shrink: 0; display: flex; align-items: center; gap: 8px;
  min-height: 48px; padding: 9px 12px; border-bottom: 1px solid var(--border, #293140); }
.tree-head strong { flex: 1; color: var(--muted-2, #697286); font-size: 10px; text-transform: uppercase; letter-spacing: .08em; }
.tree-list { flex: 1; min-height: 0; overflow: auto; padding: 6px; }
.tree-section { padding: 10px 8px 4px; color: var(--muted-2, #697286); font-size: 10px; text-transform: uppercase; letter-spacing: .08em; }
.tree-row { display: flex; align-items: center; gap: 6px; min-height: 31px; padding: 5px 8px;
  border: 1px solid transparent; border-radius: var(--r-md, 7px); cursor: pointer; font-size: var(--fs-sm, 12px); }
.tree-row:hover { background: var(--bg-3, #181e29); }
.tree-row.active { background: var(--panel, #10131a); border-color: var(--border-strong, #384153); }
.tree-row.drop-target { border-color: var(--accent, #31d6a6); background: color-mix(in srgb, var(--accent, #31d6a6) 12%, transparent); }
.tree-row .twist { width: 12px; flex: 0 0 12px; color: var(--muted-2, #697286); }
.tree-row .label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tree-row .count { color: var(--muted-2, #697286); font: 10px var(--font-mono, ui-monospace, monospace); }
.main { min-width: 0; min-height: 0; display: flex; flex-direction: column; background: var(--panel, #10131a); }
.crumbs { display: flex; align-items: center; flex-wrap: wrap; min-width: 0; gap: 3px; font-size: var(--fs-sm, 12px); }
.crumb { padding: 3px 5px; border-radius: 4px; color: var(--muted, #8c94a6); cursor: pointer; }
.crumb:hover { color: var(--text, #e8ecf3); background: var(--bg-3, #181e29); }
.crumb.current { color: var(--text, #e8ecf3); font-weight: 600; cursor: default; }
.filter { display: flex; gap: 2px; padding: 3px; background: var(--bg-2, #0c1017); border: 1px solid var(--border, #293140); border-radius: 7px; }
.filter button { border: 0; border-radius: 5px; padding: 5px 8px; color: var(--muted, #8c94a6); background: none; font-size: 10px; }
.filter button.active { color: var(--text, #e8ecf3); background: var(--panel-2, #161b25); }
.interrupt-toggle { white-space: nowrap; }
.new-automation { white-space: nowrap; }
.cards { flex: 1; min-height: 0; overflow: auto; display: grid; grid-template-columns: repeat(auto-fill, minmax(285px, 1fr));
  align-content: start; gap: 12px; padding: 14px; }
.empty { grid-column: 1 / -1; padding: 38px 22px; text-align: center; color: var(--muted, #8c94a6);
  border: 1px dashed var(--border, #293140); border-radius: var(--r-lg, 10px); }
.card { min-height: 126px; display: flex; flex-direction: column; gap: 10px; padding: 13px; cursor: pointer;
  background: var(--panel, #10131a); border: 1px solid var(--border, #293140); border-radius: var(--r-lg, 10px);
  transition: transform .1s, border-color .1s, box-shadow .1s; }
.card:hover { transform: translateY(-1px); border-color: var(--accent, #31d6a6); box-shadow: var(--shadow-md); }
.card.folder:hover { border-color: var(--warn, #ffb454); }
.card.disabled { opacity: .56; }
.card.dragging { opacity: .5; transform: scale(.98); }
.card.drop-target { border-color: var(--accent, #31d6a6); background: color-mix(in srgb, var(--accent, #31d6a6) 9%, transparent); }
.card-top { display: flex; gap: 10px; align-items: flex-start; }
.card-icon { display: grid; place-items: center; width: 36px; height: 36px; flex: 0 0 36px; border-radius: 10px;
  background: var(--bg-3, #181e29); color: var(--accent, #31d6a6); font-size: 17px; }
.card-icon.schedule { color: var(--axo-blue, #3fa9c8); }
.card-icon.event, .card-icon.folder { color: var(--warn, #ffb454); }
.card-body { min-width: 0; display: grid; gap: 4px; }
.card-title { display: flex; flex-wrap: wrap; align-items: baseline; gap: 8px; }
.card-name { font-weight: 600; font-size: var(--fs-body, 13px); overflow-wrap: anywhere; }
.trigger { padding: 2px 6px; border-radius: 4px; background: var(--bg-3, #181e29); color: var(--muted, #8c94a6);
  font: 10px var(--font-mono, ui-monospace, monospace); }
.agents, .meta { color: var(--muted-2, #697286); font-size: 10px; }
.agents { font-family: var(--font-mono, ui-monospace, monospace); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.card-foot { margin-top: auto; display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }

.editor { position: relative; height: 100%; min-height: 0; display: grid; grid-template-rows: auto minmax(0, 1fr) auto;
  gap: 8px; padding: 10px; background: var(--panel, #10131a); }
.editor-head { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; padding: 9px 11px; background: var(--panel-2, #161b25);
  border: 1px solid var(--border, #293140); border-radius: var(--r-lg, 10px); }
.editor-title { min-width: 8rem; display: grid; gap: 1px; }
.editor-title h2 { margin: 0; font-size: var(--fs-body, 13px); }
.mode { display: flex; gap: 2px; padding: 3px; background: var(--bg-2, #0c1017); border-radius: 6px; }
.mode button { padding: 4px 7px; border: 0; border-radius: 4px; color: var(--muted, #8c94a6); background: none; font-size: 10px; }
.mode button.active { color: var(--text, #e8ecf3); background: var(--panel, #10131a); }
.canvas-wrap { position: relative; min-height: 0; overflow: hidden; border: 1px solid var(--border, #293140);
  border-radius: var(--r-lg, 10px); background: var(--bg-2, #0c1017); }
.canvas-wrap ax-lattice { display: block; width: 100%; height: 100%; }
.editor[data-mode="view"] ax-node { cursor: pointer; }
.editor[data-mode="view"] ax-handle { display: none; }
ax-node { min-width: 160px; padding: 9px 12px; color: var(--text, #e8ecf3); background: var(--panel-2, #161b25);
  border: 1px solid var(--border-strong, #384153); border-radius: 9px; }
ax-node[data-node-kind="agent"] { border-left: 3px solid var(--accent, #31d6a6); }
ax-node[data-node-kind="tool"] { border-left: 3px solid var(--warn, #ffb454); }
ax-node[data-node-kind="conditional"] { border-left: 3px solid var(--accent-2, #84f1d0); }
ax-node[data-node-kind="map"] { border-left: 3px solid var(--axo-blue, #3fa9c8); }
ax-node[data-node-kind="subgraph"] { border-left: 3px solid #b46cff; }
ax-node[data-node-kind="interrupt"] { border-left: 3px solid var(--warn, #ffb454); }
ax-node[data-node-kind="text_input"] { border-left: 3px solid var(--accent-2, #84f1d0); }
.node-title { font-weight: 600; font-size: var(--fs-body, 13px); }
.node-sub, .node-input { margin-top: 2px; color: var(--muted, #8c94a6); font: 10px var(--font-mono, ui-monospace, monospace); }
.node-input { margin-top: 5px; padding-top: 5px; color: var(--axo-blue, #3fa9c8); border-top: 1px solid var(--border, #293140); }
.controls { position: absolute; top: 10px; left: 10px; }
.minimap { position: absolute; right: 10px; bottom: 10px; width: 160px; height: 100px; }
.editor-foot { color: var(--muted-2, #697286); font-size: 10px; }
.drawer { position: absolute; z-index: 55; right: 18px; top: 70px; width: min(350px, calc(100% - 36px)); max-height: calc(100% - 100px);
  display: flex; flex-direction: column; overflow: hidden; background: var(--panel-2, #161b25); border: 1px solid var(--border-strong, #384153);
  border-radius: var(--r-lg, 10px); box-shadow: var(--shadow-lg); }
.drawer-head { flex-shrink: 0; display: flex; align-items: center; gap: 8px; padding: 9px 11px; border-bottom: 1px solid var(--border, #293140); }
.drawer-head strong { flex: 1; }
.drawer-body { min-height: 0; overflow: auto; padding: 11px; }
.field { display: grid; gap: 4px; margin-bottom: 10px; }
.field label { color: var(--muted, #8c94a6); font-size: 10px; }
.divider { height: 1px; margin: 11px 0; background: var(--border, #293140); }
.branch-row { display: grid; grid-template-columns: 82px 104px minmax(0, 1fr) auto; align-items: center; gap: 5px; margin-bottom: 6px; }
.check-row { display: flex; align-items: center; gap: 6px; margin: 4px 0; font-size: var(--fs-xs, 11px); }
.runs { width: min(390px, calc(100% - 36px)); }
.run-list { min-height: 0; overflow: auto; padding: 7px; }
.run { margin-bottom: 6px; padding: 8px 9px; background: var(--panel, #10131a); border: 1px solid var(--border, #293140); border-radius: 7px; }
.run-head, .run-actions { display: flex; justify-content: space-between; align-items: center; gap: 8px; }
.run-head { cursor: pointer; }
.run-status { padding: 2px 6px; border-radius: 4px; font-size: 9px; text-transform: uppercase; }
.run-status.completed { color: var(--ok, #4ade80); background: color-mix(in srgb, var(--ok, #4ade80) 16%, transparent); }
.run-status.failed, .run-status.cancelled { color: var(--err, #ff6b6b); background: color-mix(in srgb, var(--err, #ff6b6b) 16%, transparent); }
.run-status.running { color: var(--accent, #31d6a6); background: color-mix(in srgb, var(--accent, #31d6a6) 16%, transparent); }
.run-status.interrupted { color: var(--warn, #ffb454); background: color-mix(in srgb, var(--warn, #ffb454) 16%, transparent); }
.run-steps { display: none; gap: 4px; margin-top: 8px; padding-top: 8px; border-top: 1px solid var(--border, #293140); }
.run.open .run-steps { display: grid; }
.run-step { display: grid; grid-template-columns: auto minmax(0, 1fr); align-items: center; gap: 6px;
  padding: 4px; border-radius: 4px; font: 10px var(--font-mono, ui-monospace, monospace); }
.run-step:hover { background: var(--bg-3, #181e29); }
.run-step button { border: 0; background: none; color: var(--muted, #8c94a6); font-size: 10px; }
.run-reason, .run-step-detail { white-space: pre-wrap; overflow-wrap: anywhere; line-height: 1.4; }
.run-reason { margin-top: 4px; color: var(--err, #ff6b6b); font-size: 10px; }
.run-step-detail { color: var(--err, #ff6b6b); }
.run-result { display: grid; gap: 5px; padding: 7px; border: 1px solid var(--border, #293140);
  border-radius: 5px; background: var(--bg-2, #0c1017); }
.run-result-label { color: var(--muted, #8c94a6); font-size: 9px; letter-spacing: .07em; text-transform: uppercase; }
.run-result-content { max-height: 220px; margin: 0; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere;
  color: var(--text, #e8ecf3); font: 10px/1.5 var(--font-mono, ui-monospace, monospace); }
.run-result-unavailable { padding: 4px; color: var(--muted-2, #697286); font-size: 10px; }

.popover { position: fixed; z-index: 80; width: min(420px, calc(100vw - 20px)); display: grid; gap: 7px; padding: 9px;
  background: var(--panel-2, #161b25); border: 1px solid var(--border-strong, #384153); border-radius: 10px; box-shadow: var(--shadow-lg); }
.popover-head { display: flex; align-items: center; gap: 8px; font-size: 10px; text-transform: uppercase; letter-spacing: .07em; }
.popover-tabs { display: flex; gap: 3px; padding: 3px; background: var(--bg-2, #0c1017); border-radius: 6px; }
.popover-tabs button { flex: 1; padding: 5px; border: 0; border-radius: 4px; background: none; color: var(--muted, #8c94a6); font-size: 10px; }
.popover-tabs button.active { color: var(--text, #e8ecf3); background: var(--panel, #10131a); }
.popover-list { max-height: 280px; overflow: auto; display: grid; gap: 2px; }
.popover-row { padding: 7px 9px; border-radius: 5px; cursor: pointer; }
.popover-row:hover { background: color-mix(in srgb, var(--accent, #31d6a6) 14%, transparent); }
.popover-row strong { display: block; font-size: var(--fs-xs, 11px); }
.popover-row span { display: block; margin-top: 2px; color: var(--muted, #8c94a6); font-size: 10px; }

.interrupts { position: absolute; z-index: 70; inset: 0 0 0 auto; width: min(520px, 100%); display: flex; flex-direction: column;
  background: var(--panel-2, #161b25); border-left: 1px solid var(--border-strong, #384153); box-shadow: var(--shadow-lg); }
.interrupts.full { width: 100%; }
.interrupt-list { flex: 1; min-height: 0; overflow: auto; display: grid; align-content: start; gap: 9px; padding: 10px; }
.interrupt { display: grid; gap: 8px; padding: 11px; background: var(--panel, #10131a); border: 1px solid var(--border, #293140); border-radius: 9px; }
.interrupt-message { display: grid; gap: 8px; overflow-wrap: anywhere; font-size: var(--fs-sm, 12px); line-height: 1.55; }
.interrupt-message p, .interrupt-message h3, .interrupt-message h4,
.interrupt-message ul, .interrupt-message ol, .interrupt-message pre { margin: 0; }
.interrupt-message h3, .interrupt-message h4 { color: var(--text, #e8ecf3); line-height: 1.3; }
.interrupt-message h3 { font-size: var(--fs-body, 13px); }
.interrupt-message h4 { font-size: var(--fs-sm, 12px); }
.interrupt-message ul, .interrupt-message ol { display: grid; gap: 4px; padding-left: 20px; }
.interrupt-message code {
  padding: 1px 4px; border-radius: 4px; background: var(--bg-3, #181e29);
  color: var(--accent-2, #84f1d0); font-family: var(--font-mono, ui-monospace, monospace);
}
.interrupt-message pre {
  max-height: 240px; overflow: auto; padding: 8px; border: 1px solid var(--border, #293140);
  border-radius: 6px; background: var(--bg-2, #0c1017); white-space: pre-wrap;
}
.interrupt-message pre code { padding: 0; background: transparent; color: var(--text, #e8ecf3); }
.interrupt-message a { color: var(--accent, #31d6a6); }
.interrupt-message blockquote { margin: 0; padding-left: 9px; border-left: 2px solid var(--border-strong, #384153); color: var(--muted, #8c94a6); }
.interrupt-reviews { display: grid; gap: 7px; padding-top: 8px; border-top: 1px solid var(--border, #293140); }
.interrupt-reviews > h3 { font-size: var(--fs-xs, 11px); letter-spacing: .05em; text-transform: uppercase; }
.interrupt-review-list { display: grid; gap: 8px; padding: 0; list-style: none; counter-reset: review; }
.interrupt-review {
  display: grid; gap: 6px; padding: 8px 9px; border: 1px solid var(--border, #293140);
  border-radius: 7px; background: var(--bg-2, #0c1017); counter-increment: review;
}
.interrupt-review::before { color: var(--muted, #8c94a6); font-size: 10px; content: 'Review ' counter(review); }
.interrupt-actions { display: grid; grid-template-columns: minmax(0, 1fr) auto auto; gap: 6px; align-items: stretch; }
.interrupt-actions textarea { min-height: 55px; }

.context-menu { position: fixed; z-index: 95; min-width: 185px; padding: 4px; background: var(--panel-2, #161b25);
  border: 1px solid var(--border-strong, #384153); border-radius: 7px; box-shadow: var(--shadow-lg); }
.context-item { padding: 7px 9px; border-radius: 5px; cursor: pointer; font-size: var(--fs-xs, 11px); }
.context-item:hover { background: color-mix(in srgb, var(--accent, #31d6a6) 16%, transparent); }
.context-item.danger { color: var(--err, #ff6b6b); }
.context-sep { height: 1px; margin: 4px 2px; background: var(--border, #293140); }
.overlay { position: fixed; z-index: 100; inset: 0; display: grid; place-items: center; padding: 16px; background: rgba(0,0,0,.52); }
.modal { width: min(540px, 100%); max-height: min(760px, calc(100vh - 32px)); display: flex; flex-direction: column; overflow: hidden;
  background: var(--panel-2, #161b25); border: 1px solid var(--border-strong, #384153); border-radius: 11px; box-shadow: var(--shadow-lg); }
.modal-head { padding: 13px 15px; border-bottom: 1px solid var(--border, #293140); font-weight: 600; }
.modal-body { min-height: 0; overflow: auto; display: grid; gap: 10px; padding: 14px 15px; font-size: var(--fs-sm, 12px); }
.modal-foot { display: flex; justify-content: flex-end; gap: 7px; padding: 10px 15px; border-top: 1px solid var(--border, #293140); }
.modal-error { padding: 8px 10px; color: var(--err, #ff6b6b); background: color-mix(in srgb, var(--err, #ff6b6b) 9%, transparent);
  border: 1px solid color-mix(in srgb, var(--err, #ff6b6b) 55%, transparent); border-radius: var(--r-md, 7px); overflow-wrap: anywhere; }
.starter { display: grid; gap: 3px; padding: 9px 10px; color: var(--muted, #8c94a6); background: var(--bg-2, #0c1017);
  border: 1px solid var(--border, #293140); border-radius: var(--r-md, 7px); }
.field.invalid label, .field-error { color: var(--err, #ff6b6b); }
.field.invalid .input, .field.invalid .select { border-color: var(--err, #ff6b6b); }
.field-error { font-size: 10px; line-height: 1.4; overflow-wrap: anywhere; }
.choice { display: grid; gap: 2px; padding: 8px 9px; border: 1px solid var(--border, #293140); border-radius: 7px; cursor: pointer; }
.choice.active { border-color: var(--accent, #31d6a6); background: color-mix(in srgb, var(--accent, #31d6a6) 9%, transparent); }

ax-node.live { outline: 2px solid var(--accent, #31d6a6); outline-offset: 2px; }
ax-node.complete { outline: 2px solid var(--ok, #4ade80); outline-offset: 2px; }
ax-node.paused { outline: 2px dashed var(--warn, #ffb454); outline-offset: 2px; }
ax-node.failed { outline: 2px solid var(--err, #ff6b6b); outline-offset: 2px; }
@media (max-width: 760px) {
  .explorer { grid-template-columns: 1fr; grid-template-rows: minmax(120px, 180px) minmax(0, 1fr); }
  .tree { max-height: 180px; border-right: 0; border-bottom: 1px solid var(--border, #293140); }
  .main-head { flex-wrap: wrap; }
  .cards { grid-template-columns: 1fr; }
  .filter { order: 5; width: 100%; overflow: auto; }
  .filter button { flex: 1; white-space: nowrap; }
  .minimap { display: none; }
  .branch-row { grid-template-columns: 1fr 1fr auto; }
  .branch-row .branch-value { grid-column: 1 / -1; }
  .interrupt-actions { grid-template-columns: 1fr 1fr; }
  .interrupt-actions textarea { grid-column: 1 / -1; }
}
@media (prefers-reduced-motion: reduce) { *, *::before, *::after { transition: none !important; animation: none !important; } }
`;

const TEMPLATE = `
  <div class="shell">
    <div class="errors" aria-live="polite"></div>
    <section class="explorer">
      <aside class="tree">
        <div class="tree-head"><strong>Folders</strong><button class="btn ghost new-folder" type="button">+ Folder</button></div>
        <div class="tree-list"></div>
      </aside>
      <main class="main">
        <div class="main-head">
          <nav class="crumbs" aria-label="Automation folder"></nav><span class="grow"></span>
          <button class="btn new-automation" type="button">+ Automation</button>
          <button class="btn ghost interrupt-toggle" type="button" aria-controls="pending-interrupts"
            aria-expanded="false">No interrupts</button>
          <div class="filter" role="group" aria-label="Filter Automations">
            <button class="active" type="button" data-filter="all">All</button>
            <button type="button" data-filter="manual">▶ Manual</button>
            <button type="button" data-filter="schedule">⏱ Scheduled</button>
            <button type="button" data-filter="event">⊛ Event</button>
          </div>
        </div>
        <div class="cards"></div>
      </main>
    </section>

    <section class="editor hide" data-mode="view">
      <div class="editor-head">
        <button class="btn ghost back" type="button">← Back</button>
        <div class="editor-title"><h2>—</h2><span class="small muted editor-id"></span></div>
        <span class="grow"></span><span class="trigger editor-trigger">—</span>
        <div class="mode"><button class="active" type="button" data-mode="view">◉ View</button><button type="button" data-mode="edit">✎ Edit</button></div>
        <button class="btn ghost add-node hide" type="button">+ Node</button>
        <button class="btn ghost edit-trigger hide" type="button">Trigger…</button>
        <button class="btn ghost open-runs" type="button">⟲ Runs</button>
        <button class="btn run" type="button">▶ Run</button>
      </div>
      <div class="canvas-wrap">
        <ax-lattice id="automation-settings-lattice" background="dots" snap="20" min-zoom="0.2" max-zoom="3"
          aria-label="Automation graph"></ax-lattice>
        <ax-controls class="controls"></ax-controls><ax-minimap class="minimap"></ax-minimap>
      </div>
      <div class="editor-foot">View mode · click Edit to change nodes, edges, inputs, or trigger.</div>
      <aside class="drawer inspector hide"><div class="drawer-head"><strong>Node</strong><button class="icon-btn inspector-close" type="button" title="Close node settings" aria-label="Close node settings">×</button></div><div class="drawer-body"></div></aside>
      <aside class="drawer runs hide"><div class="drawer-head"><strong>Run history</strong><button class="icon-btn runs-close" type="button" title="Close run history" aria-label="Close run history">×</button></div><div class="run-list"></div></aside>
    </section>

    <aside class="interrupts hide" id="pending-interrupts" aria-label="Pending Automation interrupts">
      <div class="drawer-head"><strong>Pending interrupts</strong><button class="icon-btn interrupts-expand" type="button"
        title="Expand pending interrupts" aria-label="Expand pending interrupts" aria-expanded="false">⛶</button><button class="icon-btn interrupts-close" type="button"
        title="Close pending interrupts" aria-label="Close pending interrupts">×</button></div>
      <div class="interrupt-list"></div>
      <div class="small muted" style="padding:10px;border-top:1px solid var(--border)">Resume guidance becomes the interrupted node's output.</div>
    </aside>

    <div class="popover add-popover hide">
      <div class="popover-head"><strong>Add a node</strong><span class="grow"></span><button class="icon-btn popover-close" type="button"
        title="Close node picker" aria-label="Close node picker">×</button></div>
      <div class="popover-tabs"><button class="active" type="button" data-kind="agent">Agents</button><button type="button" data-kind="tool">Tools</button><button type="button" data-kind="conditional">Router</button><button type="button" data-kind="flow">Flow</button></div>
      <input class="input popover-search" type="search" placeholder="Search agents…" spellcheck="false">
      <div class="popover-list"></div><div class="small muted">Click an item to add it. Esc cancels.</div>
    </div>
    <div class="context-menu hide"></div>
    <div class="overlay-layer"></div>
  </div>`;

export class AxAutomationSettings extends HTMLElement {
  #root;
  #controller;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = TEMPLATE;
    adopt(this.#root, CSS);
    this.#controller = createController(this, this.#root);
  }

  connectedCallback() { this.#controller.start(); }
  disconnectedCallback() { this.#controller.stop(); }

  get automations() { return this.#controller.automations(); }
  get folders() { return this.#controller.folders(); }
  get pendingInterrupts() { return this.#controller.pendingInterrupts(); }

  refresh() { return this.#controller.refresh(); }
  refreshInterrupts() { return this.#controller.refreshInterrupts(); }
  openAutomation(automationOrId) { return this.#controller.openAutomation(automationOrId); }
  run(automationOrId) { return this.#controller.run(automationOrId); }
  showInterrupts() { this.#controller.showInterrupts(); }
  handleFrame(frame) { this.#controller.handleFrame(frame); }
}

function createController(host, root) {
  const $ = (selector) => root.querySelector(selector);
  const $$ = (selector) => Array.from(root.querySelectorAll(selector));
  const byId = (id) => root.getElementById(id);
  const VIEWPORT_KEY = 'axo.auto.viewport.v1';
  const state = {
    connected: false,
    phase: 'idle',
    generation: 0,
    automations: [],
    folders: [],
    agents: [],
    skills: [],
    tools: null,
    filter: 'all',
    folder: { kind: 'all' },
    collapsed: new Set(),
    dragFolder: '',
    errors: new Map(),
    editor: {
      automation: null, mode: 'view', dirty: false, selectedNode: '',
      revision: 0, saving: null, saveTimer: 0, runsTimer: 0, canonical: false, openGeneration: 0,
    },
    addKind: 'agent',
    latticeReady: false,
    latticeWired: false,
    viewportTimer: 0,
    graphFrame: 0,
    graphGeneration: 0,
    graphObserver: null,
    graphObservedSize: '',
    graphLayoutPending: false,
    interrupts: [],
    interruptKeys: '',
    interruptGeneration: 0,
    interruptTimer: 0,
    frameTimer: 0,
    menuOpen: false,
    dialogSerial: 0,
  };

  const emit = (name, detail) => host.dispatchEvent(new CustomEvent(name, { detail, bubbles: true, composed: true }));
  const notify = (title, body = '', kind = 'info') => emit('notify', { title, body, kind });

  function setError(key, message, retry) {
    if (message) state.errors.set(key, { message: String(message), retry });
    else state.errors.delete(key);
    renderErrors();
  }

  function renderErrors() {
    const errors = $('.errors');
    errors.replaceChildren();
    for (const [key, item] of state.errors) {
      const row = h('div', 'error');
      row.append(h('div', 'message', item.message));
      const actions = h('div');
      if (item.retry) {
        const retry = h('button', '', 'Retry');
        retry.type = 'button';
        retry.addEventListener('click', () => item.retry());
        actions.append(retry);
      }
      const close = h('button', '', '×');
      close.type = 'button';
      close.title = 'Dismiss';
      close.setAttribute('aria-label', 'Dismiss error');
      close.addEventListener('click', () => setError(key, ''));
      actions.append(close);
      row.append(actions);
      errors.append(row);
    }
  }

  async function action(label, work, options = {}) {
    setError('action', '');
    try {
      const result = await work();
      if (options.success) notify(options.success, options.body || '', 'ok');
      return result;
    } catch (error) {
      const message = `${label}: ${error?.message || error}`;
      setError('action', message, options.retry);
      notify(label, String(error?.message || error), 'err');
      return null;
    }
  }

  async function refresh() {
    const generation = ++state.generation;
    state.phase = 'loading';
    $('.shell').classList.add('loading');
    setError('load', '');
    try {
      const [automationsResult, foldersResult, agentsResult, skillsResult] = await Promise.allSettled([
        jsonRequest('/api/automations'),
        jsonRequest('/api/automation-folders'),
        jsonRequest('/api/agents'),
        jsonRequest('/api/skills'),
      ]);
      if (generation !== state.generation) return;
      if (automationsResult.status === 'rejected') throw automationsResult.reason;
      const automations = automationsResult.value;
      if (!Array.isArray(automations)) throw new Error('The Automation API returned an invalid list.');
      state.automations = automations;
      if (foldersResult.status === 'fulfilled' && Array.isArray(foldersResult.value)) {
        state.folders = foldersResult.value;
        setError('folders', '');
      } else {
        state.folders = [];
        setError('folders', `Folders could not be loaded; showing every Automation ungrouped. ${foldersResult.reason?.message || 'invalid response'}`, () => refresh());
        state.folder = { kind: 'all' };
      }
      if (agentsResult.status === 'fulfilled' && Array.isArray(agentsResult.value)) {
        state.agents = agentsResult.value;
        setError('agents', '');
      } else {
        state.agents = [];
        setError('agents', `Agent choices could not be loaded: ${agentsResult.reason?.message || 'invalid response'}`, () => refresh());
      }
      if (skillsResult.status === 'fulfilled' && Array.isArray(skillsResult.value)) {
        state.skills = skillsResult.value;
        setError('skills', '');
      } else {
        state.skills = [];
        setError('skills', `Skill choices could not be loaded: ${skillsResult.reason?.message || 'invalid response'}`, () => refresh());
      }
      state.phase = 'ready';
      renderExplorer();
      emit('automations-change', { automations: clone(state.automations), folders: clone(state.folders) });
    } catch (error) {
      if (generation !== state.generation) return;
      state.phase = 'error';
      setError('load', `Automations could not be loaded: ${error?.message || error}`, () => refresh());
      if (!state.automations.length) renderCards();
    } finally {
      if (generation === state.generation) $('.shell').classList.remove('loading');
    }
  }

  function start() {
    if (state.connected) return;
    state.connected = true;
    wire();
    void refresh();
    void refreshInterrupts();
    scheduleInterruptPoll();
  }

  function stop() {
    state.connected = false;
    state.generation += 1;
    state.interruptGeneration += 1;
    if (state.editor.dirty) void saveAutomation();
    clearTimeout(state.interruptTimer);
    clearTimeout(state.editor.runsTimer);
    clearTimeout(state.editor.saveTimer);
    clearTimeout(state.viewportTimer);
    clearTimeout(state.frameTimer);
    cancelAnimationFrame(state.graphFrame);
    state.graphFrame = 0;
    state.graphGeneration += 1;
    state.graphObserver?.disconnect();
    state.graphObserver = null;
    state.graphObservedSize = '';
    hideContextMenu();
    closeAddPopover();
  }

  function automations() { return clone(state.automations); }
  function folders() { return clone(state.folders); }
  function pendingInterrupts() { return clone(state.interrupts); }

  let wired = false;
  function wire() {
    if (wired) return;
    wired = true;
    $('.new-folder').addEventListener('click', () => void newFolder());
    $('.new-automation').addEventListener('click', () => void createAutomation());
    $$('.filter button').forEach((button) => button.addEventListener('click', () => {
      state.filter = button.dataset.filter;
      $$('.filter button').forEach((candidate) => candidate.classList.toggle('active', candidate === button));
      renderCards();
    }));
    $('.interrupt-toggle').addEventListener('click', () => {
      setInterruptPanel($('.interrupts').classList.contains('hide'), { focus: true });
    });
    $('.interrupts-expand').addEventListener('click', () => {
      const panel = $('.interrupts');
      const expanded = !panel.classList.contains('full');
      panel.classList.toggle('full', expanded);
      $('.interrupts-expand').textContent = expanded ? '⤡' : '⛶';
      $('.interrupts-expand').title = expanded ? 'Exit expanded pending interrupts' : 'Expand pending interrupts';
      $('.interrupts-expand').setAttribute('aria-label', expanded
        ? 'Exit expanded pending interrupts'
        : 'Expand pending interrupts');
      $('.interrupts-expand').setAttribute('aria-expanded', String(expanded));
    });
    $('.interrupts-close').addEventListener('click', () => setInterruptPanel(false, { restoreFocus: true }));
    $('.back').addEventListener('click', () => void closeEditor());
    $('.run').addEventListener('click', () => {
      if (state.editor.automation) void prepareRun(state.editor.automation);
    });
    $('.open-runs').addEventListener('click', () => void toggleRuns());
    $('.runs-close').addEventListener('click', closeRuns);
    $('.inspector-close').addEventListener('click', closeInspector);
    $$('.mode button').forEach((button) => button.addEventListener('click', () => setEditorMode(button.dataset.mode)));
    $('.add-node').addEventListener('click', openAddPopover);
    $('.edit-trigger').addEventListener('click', () => void editTrigger());
    $('.popover-close').addEventListener('click', closeAddPopover);
    $('.popover-search').addEventListener('input', (event) => renderAddList(event.target.value));
    $$('.popover-tabs button').forEach((button) => button.addEventListener('click', () => {
      state.addKind = button.dataset.kind;
      $$('.popover-tabs button').forEach((candidate) => candidate.classList.toggle('active', candidate === button));
      $('.popover-search').placeholder = button.dataset.kind === 'agent' ? 'Search agents…'
        : button.dataset.kind === 'tool' ? 'Search tools…'
          : button.dataset.kind === 'flow' ? 'Search flow nodes…' : 'Search routers…';
      renderAddList($('.popover-search').value);
    }));
    root.addEventListener('dragstart', (event) => {
      state.dragFolder = event.target?.closest?.('[data-folder-path]')?.dataset?.folderPath || '';
    });
    root.addEventListener('dragend', () => { state.dragFolder = ''; });
    root.addEventListener('keydown', (event) => {
      if (event.key !== 'Escape') return;
      let handled = true;
      if (!$('.add-popover').classList.contains('hide')) closeAddPopover();
      else if (!$('.context-menu').classList.contains('hide')) hideContextMenu();
      else if (!$('.interrupts').classList.contains('hide')) setInterruptPanel(false, { restoreFocus: true });
      else if (!$('.inspector').classList.contains('hide')) closeInspector();
      else if (!$('.runs').classList.contains('hide')) closeRuns();
      else handled = false;
      if (handled) { event.preventDefault(); event.stopPropagation(); }
    });
  }

  return {
    start, stop, refresh, refreshInterrupts, automations, folders, pendingInterrupts,
    openAutomation, run, showInterrupts, handleFrame,
  };

  // The rest of the controller is declared below. Function declarations are
  // hoisted, keeping the public surface above easy to audit.

  function projectAutomation(automation) {
    const triggerKind = automation.trigger?.kind || 'manual';
    const kind = triggerKind === 'schedule' ? 'schedule'
      : triggerKind === 'on_event' ? 'event'
        : triggerKind === 'on_skill' ? 'skill' : 'manual';
    const triggerLabel = triggerKind === 'schedule' ? `every ${automation.trigger?.every || '?'}`
      : triggerKind === 'on_event' ? `on ${automation.trigger?.event || '?'}`
        : triggerKind === 'on_skill' ? `on skill ${automation.trigger?.skill_id || '?'}` : 'manual';
    return {
      id: automation.id,
      name: automation.name || automation.id,
      kind,
      triggerLabel,
      enabled: automation.enabled !== false,
      agents: (automation.nodes || [])
        .filter((node) => node.kind?.type === 'agent')
        .map((node) => node.kind.agent_id),
      raw: automation,
    };
  }

  function projectedAutomations() {
    return state.automations.map(projectAutomation).sort((left, right) => {
      if (left.enabled !== right.enabled) return left.enabled ? -1 : 1;
      return left.name.localeCompare(right.name);
    });
  }

  function renderExplorer() {
    renderTree();
    renderCrumbs();
    renderCards();
  }

  function folderMatches(automation) {
    if (state.folder.kind === 'all') return true;
    if (state.folder.kind === 'unfiled') return !automation.folder;
    if (state.folder.kind === 'folder') return (automation.folder || '') === state.folder.path;
    return true;
  }

  function childFolders(parentPath) {
    const prefix = parentPath ? `${parentPath}/` : '';
    const depth = parentPath ? parentPath.split('/').length : 0;
    return state.folders.filter((folder) => parentPath
      ? folder.path.startsWith(prefix) && folder.path.split('/').length === depth + 1
      : !folder.path.includes('/'));
  }

  function currentFolderPath() {
    return state.folder.kind === 'folder' ? state.folder.path : null;
  }

  function renderCards() {
    const cards = $('.cards');
    cards.replaceChildren();
    const all = projectedAutomations();
    const rows = all.filter((automation) => folderMatches(automation.raw))
      .filter((automation) => state.filter === 'all' || automation.kind === state.filter);
    const subfolders = state.folder.kind === 'folder' ? childFolders(state.folder.path)
      : state.folder.kind === 'unfiled' ? childFolders(null) : [];
    if (!subfolders.length && !rows.length) {
      if (!all.length && !state.folders.length) {
        const empty = h('div', 'empty');
        empty.append(h('strong', '', 'No Automations yet'));
        empty.append(h('div', 'small muted', 'Create a runnable starter graph, then edit its nodes and trigger.'));
        const create = h('button', 'btn', '+ Automation');
        create.type = 'button';
        create.style.marginTop = '12px';
        create.addEventListener('click', () => void createAutomation());
        empty.append(create);
        cards.append(empty);
      } else {
        const message = state.folder.kind === 'folder'
          ? 'This folder is empty. Create an Automation here, or move an Automation or sub-folder into it.'
          : state.folder.kind === 'unfiled'
            ? 'There are no unfiled Automations or top-level folders.'
            : 'No Automations match this filter.';
        cards.append(h('div', 'empty', message));
      }
      attachDropTarget(cards, currentFolderPath());
      return;
    }
    subfolders.forEach((folder) => cards.append(folderCard(folder)));
    rows.forEach((automation) => cards.append(automationCard(automation)));
    attachDropTarget(cards, currentFolderPath());
  }

  function folderCard(folder) {
    const card = h('article', 'card folder');
    card.tabIndex = 0;
    card.draggable = true;
    card.dataset.folderPath = folder.path;
    card.addEventListener('dragstart', (event) => {
      event.dataTransfer.setData('application/x-axo-folder', folder.path);
      event.dataTransfer.effectAllowed = 'move';
      state.dragFolder = folder.path;
      card.classList.add('dragging');
    });
    card.addEventListener('dragend', () => { card.classList.remove('dragging'); state.dragFolder = ''; });
    const top = h('div', 'card-top');
    top.append(h('div', 'card-icon folder', '📁'));
    const body = h('div', 'card-body');
    const title = h('div', 'card-title');
    title.append(h('span', 'card-name', folder.name?.trim() || folder.path.split('/').pop()), h('span', 'trigger', 'folder'));
    body.append(title);
    const descendantFolders = state.folders.filter((candidate) => candidate.path.startsWith(`${folder.path}/`)).length;
    const automationCount = state.automations.filter((automation) => {
      const path = automation.folder || '';
      return path === folder.path || path.startsWith(`${folder.path}/`);
    }).length;
    body.append(h('div', 'agents', `${automationCount} Automation${automationCount === 1 ? '' : 's'}${descendantFolders ? ` · ${descendantFolders} sub-folder${descendantFolders === 1 ? '' : 's'}` : ''}`));
    top.append(body);
    card.append(top);
    const foot = h('div', 'card-foot');
    foot.append(h('span', 'meta', folder.path), h('span', 'grow'));
    const open = h('button', 'btn ghost', 'Open →');
    open.type = 'button';
    open.addEventListener('click', (event) => { event.stopPropagation(); selectFolder(folder.path); });
    foot.append(open);
    card.append(foot);
    card.addEventListener('click', () => selectFolder(folder.path));
    card.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); selectFolder(folder.path); }
    });
    card.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      showFolderMenu(folder, event.clientX, event.clientY);
    });
    attachDropTarget(card, folder.path);
    return card;
  }

  function automationCard(automation) {
    const card = h('article', `card${automation.enabled ? '' : ' disabled'}`);
    card.tabIndex = 0;
    card.draggable = true;
    card.dataset.automationId = automation.id;
    card.addEventListener('dragstart', (event) => {
      event.dataTransfer.setData('application/x-axo-automation', automation.id);
      event.dataTransfer.effectAllowed = 'move';
      card.classList.add('dragging');
    });
    card.addEventListener('dragend', () => card.classList.remove('dragging'));
    const icon = automation.kind === 'manual' ? '▶' : automation.kind === 'schedule' ? '⏱' : automation.kind === 'skill' ? '◆' : '⊛';
    const top = h('div', 'card-top');
    top.append(h('div', `card-icon ${automation.kind}`, icon));
    const body = h('div', 'card-body');
    const title = h('div', 'card-title');
    title.append(h('span', 'card-name', automation.name), h('span', 'trigger', automation.triggerLabel));
    body.append(title, h('div', 'agents', automation.agents.length ? automation.agents.join(' → ') : '(no agents)'));
    top.append(body);
    card.append(top);
    const foot = h('div', 'card-foot');
    const referenceIssue = referenceIssueSummary(automation.raw);
    const meta = [automation.enabled ? '' : 'paused', referenceIssue ? 'needs setup' : '', automation.raw.folder ? `📁 ${automation.raw.folder}` : ''].filter(Boolean).join(' · ');
    foot.append(h('span', 'meta', meta), h('span', 'grow'));
    const run = h('button', 'btn', '▶ Run');
    run.type = 'button';
    run.disabled = Boolean(referenceIssue);
    run.title = referenceIssue || '';
    run.addEventListener('click', (event) => { event.stopPropagation(); void runFromCard(automation); });
    foot.append(run);
    card.append(foot);
    card.addEventListener('click', () => void openAutomation(automation.id));
    card.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); void openAutomation(automation.id); }
    });
    card.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      showAutomationMenu(automation, event.clientX, event.clientY);
    });
    return card;
  }

  async function runFromCard(automation) {
    const canonical = await action('Run failed', () => jsonRequest(`/api/automations/${encodeURIComponent(automation.id)}`));
    if (!canonical) return;
    automation = projectAutomation(canonical);
    if (automation.kind === 'manual') {
      await prepareRun(canonical);
      return;
    }
    await runAutomation(canonical, { input: savedTriggerInput(canonical), inputs: {} });
  }

  async function run(automationOrId) {
    const id = typeof automationOrId === 'object' ? automationOrId?.id : automationOrId;
    const automation = id
      ? await action('Run failed', () => jsonRequest(`/api/automations/${encodeURIComponent(id)}`))
      : null;
    if (!automation) return null;
    const projected = projectAutomation(automation);
    if (projected.kind === 'manual') return prepareRun(automation);
    return runAutomation(automation, { input: savedTriggerInput(automation), inputs: {} });
  }

  function savedTriggerInput(automation) {
    const trigger = automation?.trigger || {};
    return trigger.kind === 'schedule' || trigger.kind === 'on_event' ? trigger.input || '' : '';
  }

  function showInterrupts() {
    setInterruptPanel(true, { focus: true });
    void refreshInterrupts();
  }

  function setInterruptPanel(open, { focus = false, restoreFocus = false } = {}) {
    const panel = $('.interrupts');
    panel.classList.toggle('hide', !open);
    $('.interrupt-toggle').setAttribute('aria-expanded', String(open));
    if (open) {
      renderInterrupts();
      if (focus) queueMicrotask(() => $('.interrupts-close')?.focus());
    } else if (restoreFocus) {
      queueMicrotask(() => $('.interrupt-toggle')?.focus());
    }
  }

  function selectFolder(path) {
    state.folder = { kind: 'folder', path };
    renderExplorer();
  }

  function renderTree() {
    const tree = $('.tree-list');
    tree.replaceChildren();
    tree.append(treeRow({
      label: 'All Automations', icon: '▦', count: state.automations.length,
      active: state.folder.kind === 'all',
      select: () => { state.folder = { kind: 'all' }; renderExplorer(); },
    }));
    tree.append(treeRow({
      label: 'Unfiled', icon: '○', count: state.automations.filter((automation) => !automation.folder).length,
      active: state.folder.kind === 'unfiled', dropPath: null,
      select: () => { state.folder = { kind: 'unfiled' }; renderExplorer(); },
    }));
    if (!state.folders.length) return;
    tree.append(h('div', 'tree-section', 'Folders'));
    const sorted = [...state.folders].sort((left, right) => left.path.localeCompare(right.path));
    for (const folder of sorted) {
      let parent = folder.path.split('/').slice(0, -1).join('/');
      let hidden = false;
      while (parent) {
        if (state.collapsed.has(parent)) { hidden = true; break; }
        parent = parent.split('/').slice(0, -1).join('/');
      }
      if (hidden) continue;
      const children = sorted.some((candidate) => candidate.path.startsWith(`${folder.path}/`));
      const count = state.automations.filter((automation) => {
        const path = automation.folder || '';
        return path === folder.path || path.startsWith(`${folder.path}/`);
      }).length;
      tree.append(treeRow({
        label: folder.name?.trim() || folder.path.split('/').pop(),
        icon: '📁', count, depth: folder.path.split('/').length - 1,
        active: state.folder.kind === 'folder' && state.folder.path === folder.path,
        twist: children ? (state.collapsed.has(folder.path) ? '▸' : '▾') : '',
        twistAction: children ? () => {
          if (state.collapsed.has(folder.path)) state.collapsed.delete(folder.path);
          else state.collapsed.add(folder.path);
          renderTree();
        } : null,
        dropPath: folder.path,
        dragPath: folder.path,
        select: () => selectFolder(folder.path),
        context: (x, y) => showFolderMenu(folder, x, y),
      }));
    }
  }

  function treeRow(options) {
    const row = h('div', `tree-row${options.active ? ' active' : ''}`);
    row.tabIndex = 0;
    if (options.depth) row.style.paddingLeft = `${8 + options.depth * 12}px`;
    const twist = h('span', 'twist', options.twist || '');
    if (options.twistAction) twist.addEventListener('click', (event) => { event.stopPropagation(); options.twistAction(); });
    row.append(twist, h('span', '', options.icon || ''), h('span', 'label', options.label), h('span', 'count', String(options.count ?? '')));
    row.addEventListener('click', options.select);
    row.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); options.select(); }
    });
    if (options.context) row.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      options.context(event.clientX, event.clientY);
    });
    if ('dropPath' in options) attachDropTarget(row, options.dropPath);
    if (options.dragPath) {
      row.draggable = true;
      row.dataset.folderPath = options.dragPath;
      row.addEventListener('dragstart', (event) => {
        event.dataTransfer.setData('application/x-axo-folder', options.dragPath);
        event.dataTransfer.effectAllowed = 'move';
        state.dragFolder = options.dragPath;
        row.classList.add('dragging');
      });
      row.addEventListener('dragend', () => { state.dragFolder = ''; row.classList.remove('dragging'); });
    }
    return row;
  }

  function attachDropTarget(element, destination) {
    element.addEventListener('dragover', (event) => {
      const types = Array.from(event.dataTransfer?.types || []);
      const automation = types.includes('application/x-axo-automation');
      const folder = types.includes('application/x-axo-folder');
      if (!automation && !folder) return;
      if (folder && state.dragFolder && (destination === state.dragFolder || destination?.startsWith(`${state.dragFolder}/`))) return;
      event.preventDefault();
      element.classList.add('drop-target');
    });
    element.addEventListener('dragleave', () => element.classList.remove('drop-target'));
    element.addEventListener('drop', (event) => {
      event.preventDefault();
      event.stopPropagation();
      element.classList.remove('drop-target');
      const folderPath = event.dataTransfer.getData('application/x-axo-folder');
      if (folderPath) { void moveFolder(folderPath, destination); return; }
      const automationId = event.dataTransfer.getData('application/x-axo-automation');
      if (automationId) void moveAutomation(automationId, destination);
    });
  }

  function renderCrumbs() {
    const crumbs = $('.crumbs');
    crumbs.replaceChildren();
    if (state.folder.kind === 'all' || state.folder.kind === 'unfiled') {
      crumbs.append(h('span', 'crumb current', state.folder.kind === 'all' ? 'All Automations' : 'Unfiled'));
      return;
    }
    const all = h('span', 'crumb', 'All');
    all.addEventListener('click', () => { state.folder = { kind: 'all' }; renderExplorer(); });
    crumbs.append(all);
    let path = '';
    state.folder.path.split('/').forEach((part, index, parts) => {
      crumbs.append(h('span', 'muted', '›'));
      path = path ? `${path}/${part}` : part;
      const target = path;
      const crumb = h('span', `crumb${index === parts.length - 1 ? ' current' : ''}`, part);
      if (index !== parts.length - 1) crumb.addEventListener('click', () => selectFolder(target));
      crumbs.append(crumb);
    });
  }

  async function moveFolder(source, destination) {
    const basename = source.split('/').pop();
    const next = destination ? `${destination}/${basename}` : basename;
    if (next === source) return;
    if (destination && (destination === source || destination.startsWith(`${source}/`))) {
      setError('action', `Cannot move “${source}” inside itself.`);
      return;
    }
    const result = await action('Move failed', () => jsonRequest('/api/automation-folders', {
      method: 'PATCH', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ old_path: source, new_path: next }),
    }), { success: 'Folder moved', body: `${source} → ${next}` });
    if (result == null) return;
    if (state.folder.kind === 'folder') {
      if (state.folder.path === source) state.folder = { kind: 'folder', path: next };
      else if (state.folder.path.startsWith(`${source}/`)) state.folder = { kind: 'folder', path: `${next}/${state.folder.path.slice(source.length + 1)}` };
    }
    await refresh();
  }

  async function moveAutomation(id, folder) {
    const result = await action('Move failed', () => jsonRequest(`/api/automations/${encodeURIComponent(id)}/move`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ folder }),
    }), { success: 'Automation moved', body: folder ? `Into ${folder}` : 'To Unfiled' });
    if (result != null) await refresh();
  }

  async function newFolder() {
    const parent = state.folder.kind === 'folder' ? state.folder.path : '';
    const value = await promptDialog({
      title: parent ? `New folder inside ${parent}` : 'New folder',
      label: parent ? 'Folder name' : 'Path (use / for nesting)',
      placeholder: parent ? 'spec-reviews' : 'client/spec-reviews',
    });
    if (!value?.trim()) return;
    const path = parent ? `${parent}/${value.trim().replace(/^\/+|\/+$/g, '')}` : value.trim();
    const result = await action('Create folder failed', () => jsonRequest('/api/automation-folders', {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ path }),
    }), { success: 'Folder created', body: path });
    if (result != null) {
      state.folder = { kind: 'folder', path };
      await refresh();
    }
  }

  async function createAutomation() {
    const created = await createAutomationDialog();
    if (!created) return;
    notify('Automation created', created.name || created.id, 'ok');
    if (created.folder) state.folder = { kind: 'folder', path: created.folder };
    else if (state.folder.kind !== 'all') state.folder = { kind: 'unfiled' };
    await refresh();
    if (!(await openAutomation(created.id))) return;
    setEditorMode('edit');
    openInspector('agent');
    requestAnimationFrame(() => $('.inspector input, .inspector select, .add-node')?.focus());
  }

  function createAutomationDialog() {
    return new Promise((resolve) => {
      const previousFocus = root.activeElement;
      const parts = modalBase({ title: 'New Automation' });
      const error = h('div', 'modal-error hide');
      error.setAttribute('role', 'alert');
      parts.content.append(error);

      const addField = (label, control, hint = '') => {
        const field = h('div', 'field');
        field.append(h('label', '', label), control);
        if (hint) field.append(h('span', 'small muted', hint));
        parts.content.append(field);
        return control;
      };

      const name = h('input', 'input');
      name.type = 'text';
      name.placeholder = 'Review release readiness';
      name.autocomplete = 'off';
      addField('Name', name, 'Shown on the Automation card and editor.');

      const id = h('input', 'input');
      id.type = 'text';
      id.placeholder = 'review-release-readiness';
      id.autocomplete = 'off';
      id.spellcheck = false;
      addField('ID', id, 'Lowercase letters, numbers, hyphens, and underscores. Used by the API.');
      let idWasEdited = false;
      name.addEventListener('input', () => {
        if (!idWasEdited) id.value = automationIdFromName(name.value);
      });
      id.addEventListener('input', () => { idWasEdited = true; });

      const description = h('textarea', 'input');
      description.rows = 2;
      description.placeholder = 'Optional: what this Automation does.';
      addField('Description', description);

      const folder = h('select', 'select');
      const unfiled = h('option', '', 'Unfiled');
      unfiled.value = '';
      folder.append(unfiled);
      [...state.folders].sort((left, right) => left.path.localeCompare(right.path)).forEach((candidate) => {
        const option = h('option', '', candidate.name?.trim() ? `${candidate.name} · ${candidate.path}` : candidate.path);
        option.value = candidate.path;
        option.selected = state.folder.kind === 'folder' && state.folder.path === candidate.path;
        folder.append(option);
      });
      addField('Folder', folder, 'Defaults to the folder you are currently viewing.');

      const agent = h('select', 'select');
      const noAgent = h('option', '', state.agents.length ? 'Choose an Agent' : 'No Agents available');
      noAgent.value = '';
      agent.append(noAgent);
      state.agents.forEach((candidate, index) => {
        const option = h('option', '', candidate.name && candidate.name !== candidate.id ? `${candidate.name} · ${candidate.id}` : candidate.id);
        option.value = candidate.id;
        option.selected = index === 0;
        agent.append(option);
      });
      addField('Starter Agent', agent, 'The starter graph sends its explicit input node to this Agent.');

      const triggerKind = h('select', 'select');
      [
        ['manual', '▶ Manual — run from Settings'],
        ['schedule', '⏱ Schedule — fixed interval'],
        ['on_event', '⊛ On event — match an event name'],
        ['on_skill', '◆ On Skill — match one Skill'],
      ].forEach(([value, label]) => {
        const option = h('option', '', label);
        option.value = value;
        triggerKind.append(option);
      });
      addField('Trigger', triggerKind);

      const triggerDetails = h('div');
      parts.content.append(triggerDetails);
      const triggerDraft = {
        cadence: '1h',
        eventName: '',
        skillId: state.skills[0]?.id || '',
        automaticInput: '',
      };
      const triggerControls = {};
      const syncTriggerDraft = () => {
        if (triggerControls.cadence) triggerDraft.cadence = triggerControls.cadence.value;
        if (triggerControls.eventName) triggerDraft.eventName = triggerControls.eventName.value;
        if (triggerControls.skillId) triggerDraft.skillId = triggerControls.skillId.value;
        if (triggerControls.automaticInput) triggerDraft.automaticInput = triggerControls.automaticInput.value;
      };
      const renderTriggerDetails = () => {
        triggerDetails.replaceChildren();
        Object.keys(triggerControls).forEach((key) => { delete triggerControls[key]; });
        const detailField = (label, control, hint = '') => {
          const field = h('div', 'field');
          field.append(h('label', '', label), control);
          if (hint) field.append(h('span', 'small muted', hint));
          triggerDetails.append(field);
          return control;
        };
        if (triggerKind.value === 'schedule') {
          const cadence = h('input', 'input');
          cadence.value = triggerDraft.cadence;
          cadence.placeholder = '30s, 5m, 2h, or 1d';
          triggerControls.cadence = detailField('Cadence', cadence, 'A positive integer followed by s, m, h, or d. Cron is not supported.');
        } else if (triggerKind.value === 'on_event') {
          const eventName = h('input', 'input');
          eventName.value = triggerDraft.eventName;
          eventName.placeholder = 'ReviewReady';
          triggerControls.eventName = detailField('Event name', eventName, 'Matches the published event type exactly.');
        } else if (triggerKind.value === 'on_skill') {
          const skill = h('select', 'select');
          const none = h('option', '', state.skills.length ? 'Choose a Skill' : 'No Skills available');
          none.value = '';
          skill.append(none);
          state.skills.forEach((candidate) => {
            const option = h('option', '', candidate.name && candidate.name !== candidate.id ? `${candidate.name} · ${candidate.id}` : candidate.id);
            option.value = candidate.id;
            option.selected = triggerDraft.skillId === candidate.id;
            skill.append(option);
          });
          triggerControls.skillId = detailField('Skill', skill, 'Matches events produced by this Skill exactly.');
        }
        if (triggerKind.value !== 'manual') {
          const input = h('textarea', 'input');
          input.rows = 3;
          input.value = triggerDraft.automaticInput;
          input.placeholder = 'Explain what the Agent should do whenever this trigger fires.';
          triggerControls.automaticInput = detailField('Instruction for each run', input,
            triggerKind.value === 'schedule' ? 'Saved on the explicit input node and used on every interval.' : 'Saved on the input node; event or Skill payload is appended as trigger data.');
        }
      };
      triggerKind.addEventListener('change', () => {
        syncTriggerDraft();
        renderTriggerDetails();
      });
      renderTriggerDetails();

      const starter = h('div', 'starter');
      starter.append(h('strong', '', 'Starter graph'));
      starter.append(h('span', 'small', 'Input → Agent · two nodes, one edge, ready to run and extend.'));
      parts.content.append(starter);

      if (!state.agents.length) {
        const unavailable = h('div', 'modal-error', 'Agents could not be loaded. Close this dialog, retry the Agent load, then create the Automation.');
        unavailable.setAttribute('role', 'status');
        parts.content.append(unavailable);
      }

      const cancel = h('button', 'btn ghost', 'Cancel');
      cancel.type = 'button';
      const create = h('button', 'btn', 'Create Automation');
      create.type = 'button';
      create.disabled = !state.agents.length;
      parts.foot.append(cancel, create);

      let busy = false;
      let settled = false;
      const showError = (message, control) => {
        error.textContent = message;
        error.classList.remove('hide');
        control?.focus();
      };
      const cleanup = (value) => {
        if (settled) return;
        settled = true;
        parts.overlay.removeEventListener('keydown', onKey);
        parts.overlay.remove();
        previousFocus?.focus?.();
        resolve(value);
      };
      const submit = async () => {
        if (busy) return;
        syncTriggerDraft();
        const cleanName = name.value.trim();
        const cleanId = id.value.trim();
        if (!cleanName) { showError('Name is required.', name); return; }
        if (!cleanId) { showError('ID is required.', id); return; }
        if (!/^[a-z0-9][a-z0-9_-]*$/.test(cleanId)) {
          showError('ID must start with a lowercase letter or number and contain only lowercase letters, numbers, hyphens, or underscores.', id);
          return;
        }
        if (state.automations.some((automation) => automation.id === cleanId)) {
          showError(`An Automation with id “${cleanId}” already exists.`, id);
          return;
        }
        if (!agent.value) { showError('Choose an Agent for the starter graph.', agent); return; }
        if (triggerKind.value === 'schedule' && !/^[1-9]\d*[smhd]$/.test(triggerDraft.cadence.trim())) {
          showError('Cadence must be a positive integer followed by s, m, h, or d—for example 30s, 5m, 2h, or 1d.', triggerControls.cadence);
          return;
        }
        if (triggerKind.value === 'on_event' && !triggerDraft.eventName.trim()) {
          showError('Event name is required.', triggerControls.eventName);
          return;
        }
        if (triggerKind.value === 'on_skill' && !triggerDraft.skillId) {
          showError('Choose a Skill for this trigger.', triggerControls.skillId);
          return;
        }
        if (triggerKind.value !== 'manual' && !triggerDraft.automaticInput.trim()) {
          showError('Add the instruction the Agent should receive when this automatic trigger fires.', triggerControls.automaticInput);
          return;
        }
        error.classList.add('hide');
        const payload = buildStarterAutomation({
          id: cleanId,
          name: cleanName,
          description: description.value.trim(),
          agentId: agent.value,
          folder: folder.value,
          triggerKind: triggerKind.value,
          cadence: triggerDraft.cadence.trim(),
          eventName: triggerDraft.eventName.trim(),
          skillId: triggerDraft.skillId,
          automaticInput: triggerDraft.automaticInput,
        });
        busy = true;
        cancel.disabled = true;
        create.disabled = true;
        create.textContent = 'Creating…';
        try {
          const result = await jsonRequest('/api/automations', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify(payload),
          });
          cleanup(result || payload);
        } catch (requestError) {
          busy = false;
          cancel.disabled = false;
          create.disabled = false;
          create.textContent = 'Create Automation';
          showError(`Automation could not be created: ${requestError?.message || requestError}`, id);
          notify('Create Automation failed', String(requestError?.message || requestError), 'err');
        }
      };
      const onKey = (event) => {
        if (event.key === 'Escape') {
          event.preventDefault();
          event.stopPropagation();
          if (!busy) cleanup(null);
          return;
        }
        if (event.key === 'Enter' && !event.shiftKey && !['TEXTAREA', 'SELECT', 'BUTTON'].includes(event.target.tagName)) {
          event.preventDefault();
          event.stopPropagation();
          void submit();
          return;
        }
        if (event.key !== 'Tab') return;
        const focusable = modalFocusables(parts.modal);
        if (!focusable.length) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (event.shiftKey && event.target === first) { event.preventDefault(); last.focus(); }
        else if (!event.shiftKey && event.target === last) { event.preventDefault(); first.focus(); }
      };
      parts.overlay.addEventListener('keydown', onKey);
      cancel.addEventListener('click', () => { if (!busy) cleanup(null); });
      create.addEventListener('click', () => void submit());
      parts.overlay.addEventListener('pointerdown', (event) => {
        if (event.target !== parts.overlay || busy) return;
        event.preventDefault();
        event.stopPropagation();
        cleanup(null);
      });
      requestAnimationFrame(() => name.focus());
    });
  }

  function showAutomationMenu(automation, x, y) {
    showContextMenu(x, y, [
      { label: '▶ Run now', run: () => runFromCard(automation) },
      { label: 'Open editor', run: () => openAutomation(automation.id) },
      { separator: true },
      { label: 'Move to…', run: () => chooseAutomationFolder(automation.id) },
      { label: automation.enabled ? 'Pause' : 'Enable', run: () => toggleAutomation(automation) },
      { separator: true },
      { label: 'Delete…', danger: true, run: () => deleteAutomation(automation) },
    ]);
  }

  function showFolderMenu(folder, x, y) {
    showContextMenu(x, y, [
      { label: 'Rename folder…', run: () => renameFolder(folder) },
      { label: 'New sub-folder…', run: () => newSubfolder(folder) },
      { separator: true },
      { label: 'Delete folder…', danger: true, run: () => deleteFolder(folder) },
    ]);
  }

  function showContextMenu(x, y, items) {
    hideContextMenu();
    const menu = $('.context-menu');
    menu.replaceChildren();
    for (const item of items) {
      if (item.separator) { menu.append(h('div', 'context-sep')); continue; }
      const row = h('div', `context-item${item.danger ? ' danger' : ''}`, item.label);
      row.tabIndex = 0;
      const choose = () => {
        hideContextMenu();
        try { void item.run?.(); } catch (error) { setError('action', String(error?.message || error)); }
      };
      row.addEventListener('click', choose);
      row.addEventListener('keydown', (event) => {
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); choose(); }
      });
      menu.append(row);
    }
    menu.classList.remove('hide');
    const bounds = menu.getBoundingClientRect();
    menu.style.left = `${x + bounds.width > innerWidth - 6 ? Math.max(6, x - bounds.width) : x}px`;
    menu.style.top = `${y + bounds.height > innerHeight - 6 ? Math.max(6, y - bounds.height) : y}px`;
    state.menuOpen = true;
    setTimeout(() => {
      document.addEventListener('pointerdown', contextOutside, true);
      window.addEventListener('resize', hideContextMenu);
      window.addEventListener('scroll', hideContextMenu, true);
    });
  }

  function hideContextMenu() {
    if (!state.menuOpen) return;
    state.menuOpen = false;
    $('.context-menu')?.classList.add('hide');
    document.removeEventListener('pointerdown', contextOutside, true);
    window.removeEventListener('resize', hideContextMenu);
    window.removeEventListener('scroll', hideContextMenu, true);
  }

  function contextOutside(event) {
    if (!$('.context-menu')?.contains(event.composedPath()[0])) hideContextMenu();
  }

  async function toggleAutomation(automation) {
    const canonical = await action('Update failed', () => jsonRequest(`/api/automations/${encodeURIComponent(automation.id)}`));
    if (!canonical) return;
    const updated = { ...canonical, enabled: !canonical.enabled };
    const result = await action('Update failed', () => jsonRequest(`/api/automations/${encodeURIComponent(automation.id)}`, {
      method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(updated),
    }), { success: updated.enabled ? 'Automation enabled' : 'Automation paused', body: automation.name });
    if (result != null) await refresh();
  }

  async function deleteAutomation(automation) {
    const confirmed = await confirmDialog({
      title: 'Delete this Automation?',
      body: `“${automation.name}” will be removed. This cannot be undone.`,
      okLabel: 'Delete', danger: true,
    });
    if (!confirmed) return;
    const result = await action('Delete failed', () => jsonRequest(`/api/automations/${encodeURIComponent(automation.id)}`, { method: 'DELETE' }), {
      success: 'Automation deleted', body: automation.name,
    });
    if (result != null) await refresh();
  }

  async function chooseAutomationFolder(id) {
    const choices = [
      { value: '__root__', label: 'Unfiled (root)', sub: 'No folder' },
      ...state.folders.map((folder) => ({ value: folder.path, label: folder.path, sub: folder.name && folder.name !== folder.path.split('/').pop() ? folder.name : '' })),
      { value: '__new__', label: 'Create new folder…', sub: '' },
    ];
    const choice = await choiceDialog({ title: 'Move Automation', body: 'Choose its folder.', choices, okLabel: 'Move' });
    if (!choice) return;
    if (choice === '__root__') { await moveAutomation(id, null); return; }
    if (choice === '__new__') {
      const path = await promptDialog({ title: 'New folder', label: 'Path (use / for nesting)', placeholder: 'client/spec-reviews' });
      if (path?.trim()) await moveAutomation(id, path.trim());
      return;
    }
    await moveAutomation(id, choice);
  }

  async function renameFolder(folder) {
    const name = await promptDialog({ title: 'Rename folder', label: 'Display name', value: folder.name || folder.path.split('/').pop() });
    if (!name?.trim()) return;
    const result = await action('Rename failed', () => jsonRequest('/api/automation-folders', {
      method: 'PATCH', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ old_path: folder.path, new_path: folder.path, new_name: name.trim() }),
    }), { success: 'Folder renamed', body: name.trim() });
    if (result != null) await refresh();
  }

  async function newSubfolder(folder) {
    const name = await promptDialog({ title: 'New sub-folder', label: 'Name (no slashes)' });
    if (!name?.trim()) return;
    const path = `${folder.path}/${name.trim().replaceAll('/', '-')}`;
    const result = await action('Create folder failed', () => jsonRequest('/api/automation-folders', {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ path }),
    }), { success: 'Folder created', body: path });
    if (result != null) await refresh();
  }

  async function deleteFolder(folder) {
    const choice = await choiceDialog({
      title: `Delete “${folder.path}”?`,
      body: 'What should happen to its contents?',
      choices: [
        { value: 'keep', label: 'Move contents up to parent', sub: 'Automations are preserved.' },
        { value: 'recursive', label: 'Delete everything inside', sub: 'Automations and sub-folders are removed.' },
      ],
      okLabel: 'Continue',
    });
    if (!choice) return;
    if (choice === 'recursive') {
      const confirmed = await confirmDialog({
        title: 'Delete everything inside?',
        body: `All Automations under “${folder.path}” will be permanently deleted.`,
        okLabel: 'Delete all', danger: true,
      });
      if (!confirmed) return;
    }
    const result = await action('Delete folder failed', () => jsonRequest(
      `/api/automation-folders?path=${encodeURIComponent(folder.path)}&keep_contents=${choice === 'keep'}`,
      { method: 'DELETE' },
    ), { success: 'Folder deleted', body: folder.path });
    if (result == null) return;
    if (state.folder.kind === 'folder' && (state.folder.path === folder.path || state.folder.path.startsWith(`${folder.path}/`))) {
      state.folder = { kind: 'all' };
    }
    await refresh();
  }

  function modalBase({ title, body = '' }) {
    const overlay = h('div', 'overlay');
    const modal = h('div', 'modal');
    const titleNode = h('div', 'modal-head', title);
    titleNode.id = `automation-dialog-title-${++state.dialogSerial}`;
    modal.setAttribute('role', 'dialog');
    modal.setAttribute('aria-modal', 'true');
    modal.setAttribute('aria-labelledby', titleNode.id);
    modal.append(titleNode);
    const content = h('div', 'modal-body');
    if (body) content.append(h('div', '', body));
    modal.append(content);
    const foot = h('div', 'modal-foot');
    modal.append(foot);
    overlay.append(modal);
    $('.overlay-layer').append(overlay);
    return { overlay, modal, content, foot };
  }

  function modalFocusables(modal) {
    return Array.from(modal.querySelectorAll(
      'a[href], button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])',
    )).filter((control) => !control.closest('.hide') && control.getAttribute('aria-hidden') !== 'true');
  }

  function dialogPromise(spec, build, read) {
    return new Promise((resolve) => {
      const previousFocus = root.activeElement;
      const parts = modalBase(spec);
      const context = build(parts.content) || {};
      const cancel = h('button', 'btn ghost', spec.cancelLabel || 'Cancel');
      cancel.type = 'button';
      const ok = h('button', `btn${spec.danger ? ' danger' : ''}`, spec.okLabel || 'OK');
      ok.type = 'button';
      parts.foot.append(cancel, ok);
      let settled = false;
      const cleanup = (value) => {
        if (settled) return;
        settled = true;
        parts.overlay.removeEventListener('keydown', onKey);
        parts.overlay.remove();
        previousFocus?.focus?.();
        resolve(value);
      };
      const commit = () => cleanup(read(context));
      const onKey = (event) => {
        if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); cleanup(null); }
        else if (event.key === 'Enter' && !event.shiftKey
          && !['TEXTAREA', 'BUTTON', 'SELECT'].includes(event.target.tagName)
          && !event.target.closest('.choice')) {
          event.preventDefault(); event.stopPropagation(); commit();
        } else if (event.key === 'Tab') {
          const focusable = modalFocusables(parts.modal);
          if (!focusable.length) {
            event.preventDefault();
            parts.modal.focus();
            return;
          }
          const first = focusable[0];
          const last = focusable[focusable.length - 1];
          if (event.shiftKey && event.target === first) { event.preventDefault(); last.focus(); }
          else if (!event.shiftKey && event.target === last) { event.preventDefault(); first.focus(); }
        }
      };
      parts.overlay.addEventListener('keydown', onKey);
      cancel.addEventListener('click', () => cleanup(null));
      ok.addEventListener('click', commit);
      parts.overlay.addEventListener('pointerdown', (event) => {
        if (event.target !== parts.overlay) return;
        event.preventDefault();
        event.stopPropagation();
        cleanup(null);
      });
      requestAnimationFrame(() => modalFocusables(parts.modal)[0]?.focus());
    });
  }

  function confirmDialog(spec) {
    return dialogPromise(spec, () => ({}), () => true).then(Boolean);
  }

  function promptDialog(spec) {
    return dialogPromise(spec, (content) => {
      const field = h('div', 'field');
      field.append(h('label', '', spec.label || 'Value'));
      const input = spec.multiline ? h('textarea', 'input') : h('input', 'input');
      if (!spec.multiline) input.type = 'text';
      input.value = spec.value || '';
      input.placeholder = spec.placeholder || '';
      field.append(input);
      content.append(field);
      return { input };
    }, ({ input }) => input.value);
  }

  function choiceDialog(spec) {
    return dialogPromise(spec, (content) => {
      let selected = spec.choices[0]?.value || null;
      const rows = [];
      spec.choices.forEach((choice, index) => {
        const row = h('div', `choice${index === 0 ? ' active' : ''}`);
        row.tabIndex = 0;
        row.append(h('strong', '', choice.label));
        if (choice.sub) row.append(h('span', 'small muted', choice.sub));
        const choose = () => {
          selected = choice.value;
          rows.forEach((candidate) => candidate.classList.toggle('active', candidate === row));
        };
        row.addEventListener('click', choose);
        row.addEventListener('keydown', (event) => {
          if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); choose(); }
        });
        rows.push(row);
        content.append(row);
      });
      return { selected: () => selected };
    }, ({ selected }) => selected());
  }

  async function openAutomation(automationOrId) {
    const id = typeof automationOrId === 'object' ? automationOrId?.id : automationOrId;
    if (!id) return false;
    const generation = ++state.editor.openGeneration;
    setError('editor', '');
    let automation = null;
    let canonical = true;
    try {
      automation = await jsonRequest(`/api/automations/${encodeURIComponent(id)}`);
    } catch (error) {
      canonical = false;
      automation = state.automations.find((candidate) => candidate.id === id);
      if (!automation) {
        setError('editor', `Automation could not be opened: ${error?.message || error}`, () => openAutomation(id));
        return false;
      }
      setError('editor', `The latest Automation could not be read; showing the last loaded copy. ${error?.message || error}`, () => openAutomation(id));
    }
    if (generation !== state.editor.openGeneration) return false;
    state.editor.automation = clone(automation);
    state.editor.canonical = canonical;
    state.editor.dirty = false;
    state.editor.revision = 0;
    state.editor.selectedNode = '';
    state.editor.mode = 'view';
    $('.explorer').classList.add('hide');
    $('.editor').classList.remove('hide');
    $('.editor-title h2').textContent = automation.name || automation.id;
    $('.editor-id').textContent = `id: ${automation.id}`;
    $('.editor-trigger').textContent = formatTrigger(automation.trigger);
    closeInspector();
    closeRuns();
    setEditorMode('view');
    await renderGraph();
    return true;
  }

  async function closeEditor() {
    const generation = state.editor.openGeneration;
    clearTimeout(state.editor.saveTimer);
    if (state.editor.dirty && !(await saveAutomation())) return;
    if (generation !== state.editor.openGeneration) return;
    state.editor.openGeneration += 1;
    closeInspector();
    closeRuns();
    closeAddPopover();
    $('.editor').classList.add('hide');
    $('.explorer').classList.remove('hide');
    state.editor.automation = null;
    renderExplorer();
  }

  function referenceIssueForNode(automation, node) {
    const kind = node?.kind || {};
    if (kind.type === 'map') {
      if (!kind.body_node) return `Map “${node.id}” needs a body node. An empty body fails when the Map runs.`;
      const bodyNode = (automation?.nodes || []).find((candidate) => candidate.id === kind.body_node);
      if (!bodyNode) return `Map “${node.id}” points to missing node “${kind.body_node}”.`;
      if (!['agent', 'tool', 'subgraph'].includes(bodyNode.kind?.type)) {
        return `Map “${node.id}” must use an Agent, Tool, or Subgraph body; “${kind.body_node}” is ${bodyNode.kind?.type || 'unknown'}.`;
      }
    }
    if (kind.type === 'subgraph') {
      if (!kind.automation_id) return `Subgraph “${node.id}” needs an Automation to call.`;
      if (kind.automation_id === automation?.id) return `Subgraph “${node.id}” must call another Automation, not itself.`;
      if (!state.automations.some((candidate) => candidate.id === kind.automation_id)) {
        return `Subgraph “${node.id}” points to unknown Automation “${kind.automation_id}”.`;
      }
    }
    return '';
  }

  function automationReferenceIssues(automation) {
    return (automation?.nodes || [])
      .map((node) => ({ nodeId: node.id, message: referenceIssueForNode(automation, node) }))
      .filter((issue) => issue.message);
  }

  function referenceIssueSummary(automation) {
    const issues = automationReferenceIssues(automation);
    if (!issues.length) return '';
    const remainder = issues.length > 1 ? ` (+${issues.length - 1} more)` : '';
    return `${issues[0].message}${remainder}`;
  }

  function reportInvalidReferences(automation, operation) {
    const issues = automationReferenceIssues(automation);
    if (!issues.length) return false;
    const message = `${operation} blocked: ${issues.map((issue) => issue.message).join(' ')}`;
    setError('validation', message);
    notify(`${operation} blocked`, issues[0].message, 'err');
    if (state.editor.automation?.id === automation?.id) {
      state.editor.selectedNode = issues[0].nodeId;
      openInspector(issues[0].nodeId);
      setEditorMode(state.editor.mode);
    }
    return true;
  }

  function setEditorMode(mode) {
    const previousMode = state.editor.mode;
    state.editor.mode = mode === 'edit' && state.editor.canonical ? 'edit' : 'view';
    $('.editor').dataset.mode = state.editor.mode;
    $$('.mode button').forEach((button) => button.classList.toggle('active', button.dataset.mode === state.editor.mode));
    $('.add-node').classList.toggle('hide', state.editor.mode !== 'edit');
    $('.edit-trigger').classList.toggle('hide', state.editor.mode !== 'edit');
    const referenceIssue = referenceIssueSummary(state.editor.automation);
    const modeHelp = state.editor.mode === 'edit'
      ? state.editor.dirty ? 'Unsaved changes · saving automatically…' : 'Edit mode · drag nodes, wire handles, and choose a node to configure it.'
      : state.editor.canonical
        ? 'View mode · click Edit to change nodes, edges, inputs, or trigger.'
        : 'Read-only cached copy · Retry the Automation read before editing or running.';
    $('.editor-foot').textContent = referenceIssue
      ? `${modeHelp} Save and Run are blocked: ${referenceIssue}`
      : modeHelp;
    $$('.mode button[data-mode="edit"]').forEach((button) => { button.disabled = !state.editor.canonical; });
    $('.run').disabled = !state.editor.canonical || Boolean(referenceIssue);
    $('.run').title = referenceIssue || '';
    if (!referenceIssue) setError('validation', '');
    const lattice = byId('automation-settings-lattice');
    lattice?.querySelectorAll('ax-node').forEach((node) => node.setAttribute('draggable', String(state.editor.mode === 'edit')));
    syncGraphLabel(lattice, state.editor.automation);
    if (lattice && state.editor.automation && previousMode !== state.editor.mode) {
      scheduleGraphViewport(lattice, state.editor.automation, { restoreEdit: true });
    }
    if (state.editor.selectedNode && previousMode !== state.editor.mode) openInspector(state.editor.selectedNode);
  }

  function markDirty() {
    if (!state.editor.automation) return;
    state.editor.dirty = true;
    state.editor.revision += 1;
    setEditorMode(state.editor.mode);
    clearTimeout(state.editor.saveTimer);
    state.editor.saveTimer = setTimeout(() => void saveAutomation(), 600);
  }

  async function saveAutomation() {
    while (state.editor.automation && state.editor.dirty) {
      if (state.editor.saving) {
        try { await state.editor.saving; } catch {}
        if (state.errors.has('save')) return false;
        continue;
      }
      const automation = state.editor.automation;
      if (reportInvalidReferences(automation, 'Save')) return false;
      const id = automation.id;
      const revision = state.editor.revision;
      const payload = clone(automation);
      const pending = jsonRequest(`/api/automations/${encodeURIComponent(id)}`, {
        method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
      });
      state.editor.saving = pending;
      try {
        await pending;
        if (state.editor.automation?.id !== id) return true;
        if (state.editor.revision === revision) state.editor.dirty = false;
        setError('save', '');
        setEditorMode(state.editor.mode);
        const index = state.automations.findIndex((candidate) => candidate.id === id);
        if (index >= 0 && state.editor.revision === revision) state.automations[index] = payload;
        emit('automations-change', { automations: clone(state.automations), folders: clone(state.folders) });
      } catch (error) {
        setError('save', `Automation could not be saved: ${error?.message || error}`, () => saveAutomation());
        setEditorMode(state.editor.mode);
        return false;
      } finally {
        if (state.editor.saving === pending) state.editor.saving = null;
      }
    }
    return true;
  }

  async function prepareRun(automation) {
    if (!automation?.id) return;
    if (reportInvalidReferences(automation, 'Run')) return;
    const textInputs = (automation.nodes || []).filter((node) => node.kind?.type === 'text_input');
    const fromTrigger = (automation.nodes || []).some((node) => node.kind?.input?.kind === 'from_trigger');
    if (!textInputs.length && !fromTrigger) {
      await runAutomation(automation, { input: '', inputs: {} });
      return;
    }
    const result = await dialogPromise({ title: `Run · ${automation.name || automation.id}`, okLabel: '▶ Run' }, (content) => {
      const fields = [];
      if (fromTrigger) {
        const field = h('div', 'field');
        field.append(h('label', '', 'Trigger input'), h('span', 'small muted', 'Feeds every node that reads From trigger.'));
        const input = h('textarea', 'input');
        input.placeholder = 'Type the prompt for this run…';
        field.append(input);
        content.append(field);
        fields.push({ trigger: true, input });
      }
      textInputs.forEach((node) => {
        const field = h('div', 'field');
        field.append(h('label', '', node.kind.label || node.id));
        const input = node.kind.multiline ? h('textarea', 'input') : h('input', 'input');
        if (!node.kind.multiline) input.type = 'text';
        input.value = node.kind.default_value || '';
        input.placeholder = node.kind.placeholder || '';
        field.append(input);
        content.append(field);
        fields.push({ id: node.id, input });
      });
      return { fields };
    }, ({ fields }) => {
      const payload = { input: '', inputs: {} };
      fields.forEach((field) => {
        if (field.trigger) payload.input = field.input.value || '';
        else payload.inputs[field.id] = field.input.value || '';
      });
      return payload;
    });
    if (result) await runAutomation(automation, result);
  }

  async function runAutomation(automation, payload) {
    if (state.editor.automation?.id === automation.id && !state.editor.canonical) {
      notify('Run blocked', 'Reload the canonical Automation before running it.', 'err');
      return null;
    }
    if (reportInvalidReferences(automation, 'Run')) return null;
    if (state.editor.automation?.id === automation.id && state.editor.dirty) {
      clearTimeout(state.editor.saveTimer);
      if (!(await saveAutomation())) {
        notify('Run blocked', 'Save the Automation successfully before running it.', 'err');
        return null;
      }
      automation = state.editor.automation;
    }
    const response = await action('Run failed', () => jsonRequest(`/api/automations/${encodeURIComponent(automation.id)}/run`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
    }), { success: 'Automation started', body: automation.name || automation.id });
    if (response == null) return null;
    emit('run-started', { automationId: automation.id, response: clone(response) });
    if (!$('.runs').classList.contains('hide') && state.editor.automation?.id === automation.id) {
      setTimeout(() => void refreshRuns(), 350);
    }
    return response;
  }

  async function editTrigger() {
    const automation = state.editor.automation;
    if (!automation) return;
    const trigger = automation.trigger || { kind: 'manual' };
    const result = await dialogPromise({ title: 'Configure trigger', okLabel: 'Save' }, (content) => {
      const kindField = h('div', 'field');
      kindField.append(h('label', '', 'Trigger kind'));
      const kind = h('select', 'select');
      [
        ['manual', '▶ Manual — user clicks Run'],
        ['schedule', '⏱ Schedule — fire on an interval'],
        ['on_event', '⊛ On event — lattice event matches'],
        ['on_skill', '◆ On skill — a Skill is published'],
      ].forEach(([value, label]) => {
        const option = h('option', '', label); option.value = value; option.selected = trigger.kind === value; kind.append(option);
      });
      kindField.append(kind);
      content.append(kindField);
      const dynamic = h('div');
      content.append(dynamic);
      const controls = { kind, dynamic };
      const render = () => {
        dynamic.replaceChildren();
        const addField = (label, input) => { const field = h('div', 'field'); field.append(h('label', '', label), input); dynamic.append(field); return input; };
        if (kind.value === 'schedule') {
          const cadence = h('input', 'input'); cadence.value = trigger.kind === 'schedule' ? trigger.every || '1h' : '1h'; cadence.placeholder = '30s, 5m, 2h, 1d';
          controls.cadence = addField('Cadence', cadence);
          const input = h('textarea', 'input'); input.value = trigger.kind === 'schedule' ? trigger.input || '' : '';
          controls.input = addField('Default trigger input (optional)', input);
        } else if (kind.value === 'on_event') {
          const event = h('input', 'input'); event.value = trigger.kind === 'on_event' ? trigger.event || '' : ''; event.placeholder = 'AgentFailed';
          controls.event = addField('Event name', event);
          const input = h('textarea', 'input'); input.value = trigger.kind === 'on_event' ? trigger.input || '' : '';
          controls.input = addField('Default trigger input (optional)', input);
        } else if (kind.value === 'on_skill') {
          const skill = h('select', 'select');
          const none = h('option', '', 'Pick a Skill'); none.value = ''; skill.append(none);
          state.skills.forEach((candidate) => {
            const option = h('option', '', candidate.name || candidate.id); option.value = candidate.id;
            option.selected = trigger.kind === 'on_skill' && trigger.skill_id === candidate.id;
            skill.append(option);
          });
          controls.skill = addField('Skill', skill);
        }
      };
      kind.addEventListener('change', render);
      render();
      return controls;
    }, (controls) => {
      if (controls.kind.value === 'manual') return { kind: 'manual' };
      if (controls.kind.value === 'schedule') return { kind: 'schedule', every: controls.cadence.value.trim(), input: controls.input.value.trim() || null };
      if (controls.kind.value === 'on_event') return { kind: 'on_event', event: controls.event.value.trim(), input: controls.input.value.trim() || null };
      return { kind: 'on_skill', skill_id: controls.skill.value };
    });
    if (!result) return;
    if (result.kind === 'schedule' && !result.every) { setError('action', 'A schedule cadence is required.'); return; }
    if (result.kind === 'on_event' && !result.event) { setError('action', 'An event name is required.'); return; }
    if (result.kind === 'on_skill' && !result.skill_id) { setError('action', 'Choose a Skill for this trigger.'); return; }
    automation.trigger = result;
    $('.editor-trigger').textContent = formatTrigger(result);
    markDirty();
  }

  function formatTrigger(trigger) {
    if (!trigger?.kind || trigger.kind === 'manual') return '▶ manual';
    if (trigger.kind === 'schedule') return `⏱ every ${trigger.every || '?'}`;
    if (trigger.kind === 'on_event') return `⊛ on ${trigger.event || '?'}`;
    if (trigger.kind === 'on_skill') return `◆ on skill ${trigger.skill_id || '?'}`;
    return trigger.kind;
  }

  function openAddPopover() {
    const popover = $('.add-popover');
    const button = $('.add-node');
    popover.classList.remove('hide');
    popover.style.left = '-9999px';
    popover.style.top = '-9999px';
    $('.popover-search').value = '';
    renderAddList('');
    requestAnimationFrame(() => {
      const anchor = button.getBoundingClientRect();
      const bounds = popover.getBoundingClientRect();
      const top = anchor.bottom + 6 + bounds.height <= innerHeight - 8 ? anchor.bottom + 6 : Math.max(8, anchor.top - bounds.height - 6);
      const left = Math.min(anchor.left, innerWidth - bounds.width - 8);
      popover.style.left = `${Math.max(8, left)}px`;
      popover.style.top = `${top}px`;
      $('.popover-search').focus();
    });
    setTimeout(() => document.addEventListener('pointerdown', addPopoverOutside, true));
  }

  function closeAddPopover() {
    $('.add-popover')?.classList.add('hide');
    document.removeEventListener('pointerdown', addPopoverOutside, true);
  }

  function addPopoverOutside(event) {
    const path = event.composedPath();
    if (path.includes($('.add-popover')) || path.includes($('.add-node'))) return;
    closeAddPopover();
  }

  async function ensureTools() {
    if (state.tools) return state.tools;
    try {
      const response = await jsonRequest('/api/tools');
      state.tools = Array.isArray(response) ? response : Array.isArray(response?.tools) ? response.tools : [];
    } catch (error) {
      state.tools = [];
      setError('tools', `Tools could not be loaded: ${error?.message || error}`, () => { state.tools = null; renderAddList($('.popover-search').value); });
    }
    return state.tools;
  }

  function renderAddList(query = '') {
    const list = $('.popover-list');
    list.replaceChildren();
    const needle = query.trim().toLowerCase();
    const addRow = (label, description, callback) => {
      const row = h('div', 'popover-row');
      row.tabIndex = 0;
      row.append(h('strong', '', label));
      if (description) row.append(h('span', '', description));
      const choose = () => callback();
      row.addEventListener('click', choose);
      row.addEventListener('keydown', (event) => {
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); choose(); }
      });
      list.append(row);
    };
    if (state.addKind === 'agent') {
      state.agents.filter((agent) => !needle || `${agent.id} ${agent.name || ''} ${agent.team || ''} ${agent.model || ''}`.toLowerCase().includes(needle))
        .forEach((agent) => addRow(agent.name || agent.id, [agent.id, agent.team, agent.model].filter(Boolean).join(' · '), () => addNode('agent', { agentId: agent.id, label: agent.id })));
    } else if (state.addKind === 'tool') {
      list.append(h('div', 'small muted', 'Loading tools…'));
      void ensureTools().then((tools) => {
        if (state.addKind !== 'tool') return;
        list.replaceChildren();
        tools.filter((tool) => !needle || `${tool.name || tool.id} ${tool.description || ''}`.toLowerCase().includes(needle))
          .forEach((tool) => {
            const id = tool.name || tool.id;
            addRow(id, (tool.description || '').slice(0, 100), () => addNode('tool', { toolId: id, label: id }));
          });
        if (!list.children.length) list.append(h('div', 'small muted', 'No tools match.'));
      });
    } else if (state.addKind === 'conditional') {
      [
        ['If/else router', 'Two branches with editable predicates.', 'cond-2'],
        ['Multi-way switch', 'Three branches; add or remove more in the inspector.', 'cond-3'],
      ].filter(([label, description]) => !needle || `${label} ${description}`.toLowerCase().includes(needle))
        .forEach(([label, description, template]) => addRow(label, description, () => addNode('conditional', { template, label: 'router' })));
    } else {
      [
        ['Text input', 'A run-time field with an optional saved default.', 'text_input'],
        ['Map over a list', 'Run one body node for every list item.', 'map'],
        ['Subgraph', 'Call another Automation as one node.', 'subgraph'],
        ['Interrupt', 'Park the run until an operator resumes or cancels.', 'interrupt'],
      ].filter(([label, description]) => !needle || `${label} ${description}`.toLowerCase().includes(needle))
        .forEach(([label, description, kind]) => addRow(label, description, () => addNode(kind, { label: kind })));
    }
    if (!list.children.length) list.append(h('div', 'small muted', 'No nodes match.'));
  }

  function addNode(type, options) {
    const automation = state.editor.automation;
    if (!automation) return;
    automation.nodes ||= [];
    automation.edges ||= [];
    let id = options.label || type;
    let suffix = 2;
    while (automation.nodes.some((node) => node.id === id)) id = `${options.label || type}-${suffix++}`;
    const position = dropPosition(automation);
    let kind;
    if (type === 'agent') kind = { type: 'agent', agent_id: options.agentId, input: { kind: 'from_trigger' } };
    else if (type === 'tool') kind = { type: 'tool', tool_id: options.toolId, input: { kind: 'from_trigger' } };
    else if (type === 'conditional') kind = {
      type: 'conditional', input: { kind: 'from_trigger' },
      branches: options.template === 'cond-3'
        ? [{ name: 'a', when: { op: 'contains', value: '' } }, { name: 'b', when: { op: 'contains', value: '' } }, { name: 'c', when: { op: 'always' } }]
        : [{ name: 'true', when: { op: 'not_empty' } }, { name: 'false', when: { op: 'always' } }],
      default: null,
    };
    else if (type === 'map') kind = { type: 'map', input: { kind: 'from_trigger' }, body_node: '' };
    else if (type === 'subgraph') kind = { type: 'subgraph', automation_id: '', input: { kind: 'from_trigger' } };
    else if (type === 'interrupt') kind = { type: 'interrupt', input: { kind: 'literal', value: 'Please review and provide input to continue.' }, resume_strategy: 'replace' };
    else kind = { type: 'text_input', label: 'Input', default_value: null, placeholder: null, multiline: false };
    automation.nodes.push({ id, kind, position });
    markDirty();
    closeAddPopover();
    void renderGraph();
    notify('Node added', id, 'ok');
  }

  function dropPosition(automation) {
    const placed = (automation.nodes || []).filter((node) => node.position);
    if (!placed.length) return { x: 0, y: 0 };
    return {
      x: Math.max(...placed.map((node) => Number(node.position.x) || 0)) + 220,
      y: placed.reduce((sum, node) => sum + (Number(node.position.y) || 0), 0) / placed.length,
    };
  }

  async function ensureLattice() {
    if (!state.latticeReady) {
      await import('/lattice/index.js');
      await customElements.whenDefined('ax-lattice');
      state.latticeReady = true;
    }
    const lattice = byId('automation-settings-lattice');
    $('.controls').target = lattice;
    $('.minimap').target = lattice;
    if (!state.graphObserver && typeof ResizeObserver === 'function') {
      state.graphObserver = new ResizeObserver((entries) => {
        const { width = 0, height = 0 } = entries[0]?.contentRect || {};
        if (width <= 0 || height <= 0 || !state.editor.automation
          || state.editor.mode !== 'view' || $('.editor').classList.contains('hide')) return;
        const size = `${Math.round(width)}x${Math.round(height)}`;
        if (size === state.graphObservedSize) return;
        state.graphObservedSize = size;
        scheduleGraphViewport(lattice, state.editor.automation);
      });
      state.graphObserver.observe(lattice);
    }
  }

  function syncGraphLabel(lattice, automation) {
    if (!lattice) return;
    const name = automation?.name || automation?.id || 'Automation';
    const count = automation?.nodes?.length || 0;
    const mode = state.editor.mode === 'edit' ? 'Edit mode' : 'View mode';
    lattice.setAttribute('aria-label', `${name} graph. ${count} ${count === 1 ? 'node' : 'nodes'}. ${mode}.`);
  }

  function scheduleGraphViewport(lattice, automation, { autoLayout = false, restoreEdit = false } = {}) {
    const generation = ++state.graphGeneration;
    cancelAnimationFrame(state.graphFrame);
    let attempts = 4;
    const apply = () => {
      if (generation !== state.graphGeneration || state.editor.automation?.id !== automation.id) return;
      const rect = lattice.getBoundingClientRect();
      if ((rect.width <= 0 || rect.height <= 0) && attempts > 0) {
        attempts -= 1;
        state.graphFrame = requestAnimationFrame(apply);
        return;
      }
      if (rect.width <= 0 || rect.height <= 0) return;
      try {
        if (autoLayout || state.graphLayoutPending) {
          lattice.autoLayout({ direction: 'LR' });
          state.graphLayoutPending = false;
        }
        const viewport = state.editor.mode === 'edit' && restoreEdit
          ? loadViewport(automation.id)
          : null;
        if (viewport && typeof viewport.k === 'number') {
          lattice.setViewport({ x: viewport.x || 0, y: viewport.y || 0, k: viewport.k });
        } else {
          lattice.fitView({ padding: 64 });
        }
        lattice.clearHistory();
      } catch {}
      reconcilePausedNodes();
    };
    state.graphFrame = requestAnimationFrame(apply);
  }

  async function renderGraph() {
    const automation = state.editor.automation;
    if (!automation) return;
    try {
      await ensureLattice();
    } catch (error) {
      setError('lattice', `Graph editor could not be loaded: ${error?.message || error}`, () => renderGraph());
      return;
    }
    const lattice = byId('automation-settings-lattice');
    syncGraphLabel(lattice, automation);
    state.graphLayoutPending = false;
    if (!state.latticeWired) {
      state.latticeWired = true;
      lattice.addEventListener('selection-change', (event) => {
        const ids = event.detail?.ids || [];
        if (ids.length === 1 && ids[0].startsWith('automation-node-')) openInspector(ids[0].slice('automation-node-'.length));
        else closeInspector();
      });
      lattice.addEventListener('node-moveend', () => {
        if (state.editor.mode === 'edit') syncPositions();
      });
      lattice.addEventListener('edge-connect', (event) => {
        if (state.editor.mode !== 'edit') return;
        const from = (event.detail?.from?.match(/^automation-node-(.+?):out$/) || [])[1];
        const to = (event.detail?.to?.match(/^automation-node-(.+?):in$/) || [])[1];
        const current = state.editor.automation;
        if (!from || !to || !current) return;
        current.edges ||= [];
        if (!current.edges.some((edge) => edge.from === from && edge.to === to)) {
          current.edges.push({ from, to });
          markDirty();
        }
      });
      lattice.addEventListener('nodes-delete-request', (event) => {
        event.preventDefault();
        if (state.editor.mode !== 'edit') return;
        const ids = (event.detail?.ids || [])
          .filter((id) => id.startsWith('automation-node-'))
          .map((id) => id.slice('automation-node-'.length));
        if (ids.length) void deleteAutomationNodes(ids);
      });
      lattice.addEventListener('edges-delete-request', (event) => {
        event.preventDefault();
        if (state.editor.mode !== 'edit') return;
        const indexes = (event.detail?.ids || []).map((id) => Number.parseInt(String(id).replace('automation-edge-', ''), 10))
          .filter((index) => Number.isInteger(index));
        if (indexes.length) void deleteAutomationEdges(indexes);
      });
      lattice.addEventListener('viewport-change', (event) => {
        const id = state.editor.automation?.id;
        if (!id || state.editor.mode !== 'edit') return;
        clearTimeout(state.viewportTimer);
        state.viewportTimer = setTimeout(() => saveViewport(id, event.detail), 150);
      });
    }
    lattice.replaceChildren();
    (automation.nodes || []).forEach((model) => {
      const node = document.createElement('ax-node');
      node.id = `automation-node-${model.id}`;
      node.dataset.nodeKind = model.kind?.type || 'agent';
      node.setAttribute('draggable', String(state.editor.mode === 'edit'));
      if (model.position) {
        node.setAttribute('data-x', String(model.position.x));
        node.setAttribute('data-y', String(model.position.y));
      }
      const { subtitle, input } = nodeSummary(model);
      node.append(
        h('div', 'node-title', model.id),
        h('div', 'node-sub', subtitle),
        h('div', 'node-input', input),
        makeHandle('target', 'in', 'left'),
        makeHandle('source', 'out', 'right'),
      );
      lattice.append(node);
    });
    (automation.edges || []).forEach((model, index) => {
      const edge = document.createElement('ax-edge');
      edge.id = `automation-edge-${index}`;
      edge.setAttribute('from', `automation-node-${model.from}:out`);
      edge.setAttribute('to', `automation-node-${model.to}:in`);
      if (model.label) edge.setAttribute('label', model.label);
      lattice.append(edge);
    });
    const graphNodes = automation.nodes || [];
    state.graphLayoutPending = graphNodes.length > 0 && graphNodes.some((node) => (
      !node.position
      || !Number.isFinite(Number(node.position.x))
      || !Number.isFinite(Number(node.position.y))
    ));
    scheduleGraphViewport(lattice, automation, {
      autoLayout: state.graphLayoutPending,
      restoreEdit: true,
    });
  }

  function makeHandle(type, id, position) {
    const handle = document.createElement('ax-handle');
    handle.setAttribute('type', type);
    handle.setAttribute('handle-id', id);
    handle.setAttribute('position', position);
    return handle;
  }

  function nodeSummary(node) {
    const kind = node.kind || {};
    if (kind.type === 'agent') {
      const agent = state.agents.find((candidate) => candidate.id === kind.agent_id);
      return { subtitle: `🧠 agent · ${kind.agent_id || '?'}${agent?.model ? ` · ${agent.model}` : ''}`, input: formatInput(kind.input) };
    }
    if (kind.type === 'tool') return { subtitle: `🛠 tool · ${kind.tool_id || '?'}`, input: formatInput(kind.input) };
    if (kind.type === 'conditional') return { subtitle: `⨿ router · ${(kind.branches || []).length} branches`, input: formatInput(kind.input) };
    if (kind.type === 'map') return { subtitle: `⇄ map · body: ${kind.body_node || '(unset)'}`, input: formatInput(kind.input) };
    if (kind.type === 'subgraph') return { subtitle: `⊞ subgraph · ${kind.automation_id || '(unset)'}`, input: formatInput(kind.input) };
    if (kind.type === 'interrupt') return { subtitle: `⏸ interrupt · ${kind.resume_strategy || 'replace'}`, input: formatInput(kind.input) };
    if (kind.type === 'text_input') return { subtitle: `✎ input · ${kind.label || 'untitled'}`, input: kind.default_value ? `default: “${kind.default_value.slice(0, 32)}”` : '(no default)' };
    return { subtitle: kind.type || '?', input: formatInput(kind.input) };
  }

  function formatInput(input) {
    if (!input) return '';
    if (input.kind === 'from_trigger') return 'input: ⇡ from trigger';
    if (input.kind === 'literal') return `input: “${(input.value || '').slice(0, 40)}”`;
    if (input.kind === 'from_upstream') return `input: ← ${(input.nodes || []).join(', ')}`;
    if (input.kind === 'template') return 'input: template…';
    if (input.kind === 'from_map_item') return 'input: ⇄ map item';
    return '';
  }

  function syncPositions() {
    const automation = state.editor.automation;
    if (!automation) return;
    let changed = false;
    (automation.nodes || []).forEach((model) => {
      const node = byId(`automation-node-${model.id}`);
      if (!node) return;
      const position = { x: Number.parseFloat(node.getAttribute('data-x')) || 0, y: Number.parseFloat(node.getAttribute('data-y')) || 0 };
      if (!model.position || model.position.x !== position.x || model.position.y !== position.y) {
        model.position = position;
        changed = true;
      }
    });
    if (changed) markDirty();
  }

  function loadViewport(id) {
    try { return JSON.parse(localStorage.getItem(VIEWPORT_KEY) || '{}')[id] || null; } catch { return null; }
  }
  function saveViewport(id, viewport) {
    try {
      const values = JSON.parse(localStorage.getItem(VIEWPORT_KEY) || '{}');
      values[id] = viewport;
      localStorage.setItem(VIEWPORT_KEY, JSON.stringify(values));
    } catch {}
  }

  function openInspector(id) {
    const automation = state.editor.automation;
    const node = automation?.nodes?.find((candidate) => candidate.id === id);
    if (!automation || !node) return;
    state.editor.selectedNode = id;
    const inspector = $('.inspector');
    inspector.classList.remove('hide');
    $('.inspector .drawer-head strong').textContent = `Node · ${id}`;
    renderInspector(node);
  }

  function closeInspector() {
    state.editor.selectedNode = '';
    $('.inspector')?.classList.add('hide');
  }

  function renderInspector(node) {
    const body = $('.inspector .drawer-body');
    const automation = state.editor.automation;
    const kind = node.kind || {};
    const editable = state.editor.mode === 'edit';
    body.replaceChildren();
    body.append(h('div', 'small muted', nodeSummary(node).subtitle));
    const divider = () => body.append(h('div', 'divider'));
    const field = (label, control) => {
      const wrap = h('div', 'field');
      wrap.append(h('label', '', label), control);
      body.append(wrap);
      return control;
    };
    const showFieldIssue = (control, message) => {
      const wrap = control.closest('.field');
      wrap?.querySelector('.field-error')?.remove();
      wrap?.classList.toggle('invalid', Boolean(message));
      control.setAttribute('aria-invalid', String(Boolean(message)));
      control.removeAttribute('aria-describedby');
      if (!message || !wrap) return;
      const issue = h('span', 'field-error', message);
      issue.id = `automation-node-${automation.id}-${node.id}-issue`.replace(/[^a-zA-Z0-9_-]/g, '-');
      issue.setAttribute('role', 'status');
      control.setAttribute('aria-describedby', issue.id);
      wrap.append(issue);
    };
    const select = (choices, current) => {
      const control = h('select', 'select');
      choices.forEach(([value, label]) => {
        const option = h('option', '', label); option.value = value; option.selected = value === current; control.append(option);
      });
      return control;
    };

    divider();
    if (kind.type === 'agent') {
      const control = field('Agent', select(state.agents.map((agent) => [agent.id, agent.id]), kind.agent_id));
      control.addEventListener('change', () => { kind.agent_id = control.value; markDirty(); void renderGraph(); });
    } else if (kind.type === 'tool') {
      const control = field('Tool', select([[kind.tool_id || '', kind.tool_id || '(loading…)']], kind.tool_id || ''));
      void ensureTools().then((tools) => {
        control.replaceChildren();
        tools.forEach((tool) => {
          const id = tool.name || tool.id;
          const option = h('option', '', id); option.value = id; option.selected = id === kind.tool_id; control.append(option);
        });
      });
      control.addEventListener('change', () => { kind.tool_id = control.value; markDirty(); void renderGraph(); });
    } else if (kind.type === 'map') {
      const supportedBodies = new Set(['agent', 'tool', 'subgraph']);
      const options = [['', '(required — choose a runnable node)'], ...(automation.nodes || [])
        .filter((candidate) => candidate.id !== node.id && supportedBodies.has(candidate.kind?.type))
        .map((candidate) => [candidate.id, `${candidate.id} · ${candidate.kind?.type || '?'}`])];
      if (kind.body_node && !options.some(([value]) => value === kind.body_node)) {
        options.splice(1, 0, [kind.body_node, `Invalid body · ${kind.body_node}`]);
      }
      const control = field('Body node — runs once per item', select(options, kind.body_node || ''));
      const validate = () => showFieldIssue(control, referenceIssueForNode(automation, node));
      control.addEventListener('change', () => {
        kind.body_node = control.value;
        validate();
        markDirty();
        void renderGraph();
      });
      validate();
      body.append(h('div', 'small muted', 'Required: choose an Agent, Tool, or Subgraph. Use From map item in that body node to read the current item.'));
    } else if (kind.type === 'subgraph') {
      const options = [['', '(required — choose another Automation)'], ...state.automations.filter((candidate) => candidate.id !== automation.id).map((candidate) => [candidate.id, `${candidate.name || candidate.id} · ${candidate.id}`])];
      if (kind.automation_id && !options.some(([value]) => value === kind.automation_id)) {
        options.splice(1, 0, [kind.automation_id, `Unknown Automation · ${kind.automation_id}`]);
      }
      const control = field('Automation to call', select(options, kind.automation_id || ''));
      const validate = () => showFieldIssue(control, referenceIssueForNode(automation, node));
      control.addEventListener('change', () => {
        kind.automation_id = control.value;
        validate();
        markDirty();
        void renderGraph();
      });
      validate();
    } else if (kind.type === 'interrupt') {
      const control = field('Resume strategy', select([
        ['replace', 'Replace — operator value becomes this output'],
        ['append', 'Append — operator value follows the message'],
      ], kind.resume_strategy || 'replace'));
      control.addEventListener('change', () => { kind.resume_strategy = control.value; markDirty(); void renderGraph(); });
    } else if (kind.type === 'text_input') {
      renderTextInputInspector(body, node);
      renderDeleteNode(body, node);
      if (!editable) {
        body.querySelectorAll('input, select, textarea, button').forEach((control) => { control.disabled = true; });
        body.append(h('div', 'small muted', 'View mode is read-only. Click Edit to change this node.'));
      }
      return;
    }

    renderInputEditor(body, node);
    if (kind.type === 'conditional') renderConditionalEditor(body, node);
    renderDeleteNode(body, node);
    if (!editable) {
      body.querySelectorAll('input, select, textarea, button').forEach((control) => { control.disabled = true; });
      body.append(h('div', 'small muted', 'View mode is read-only. Click Edit to change this node.'));
    }
  }

  function renderTextInputInspector(body, node) {
    const kind = node.kind;
    const add = (label, control) => { const wrap = h('div', 'field'); wrap.append(h('label', '', label), control); body.append(wrap); };
    const label = h('input', 'input'); label.value = kind.label || ''; label.placeholder = 'Field name shown to the operator';
    label.addEventListener('input', () => { kind.label = label.value; markDirty(); refreshGraphNode(node); });
    add('Label', label);
    const value = h('textarea', 'input'); value.value = kind.default_value || ''; value.placeholder = 'Empty means the operator supplies it for a Manual run.';
    value.addEventListener('input', () => { kind.default_value = value.value || null; markDirty(); refreshGraphNode(node); });
    add('Default value (automatic runs)', value);
    const placeholder = h('input', 'input'); placeholder.value = kind.placeholder || ''; placeholder.placeholder = 'Bug report text…';
    placeholder.addEventListener('input', () => { kind.placeholder = placeholder.value || null; markDirty(); });
    add('Placeholder', placeholder);
    const row = h('label', 'check-row');
    const multiline = h('input'); multiline.type = 'checkbox'; multiline.checked = Boolean(kind.multiline);
    multiline.addEventListener('change', () => { kind.multiline = multiline.checked; markDirty(); });
    row.append(multiline, h('span', '', 'Multi-line field'));
    body.append(row);
  }

  function renderInputEditor(body, node) {
    const automation = state.editor.automation;
    const kind = node.kind;
    body.append(h('div', 'divider'));
    const label = h('label', 'small muted', kind.type === 'tool' ? 'Args input source' : 'Input source');
    const select = h('select', 'select');
    [
      ['from_trigger', '⇡ From trigger'],
      ['literal', '« Literal fixed value'],
      ['from_upstream', '← From upstream nodes'],
      ['template', '✎ Template'],
      ['from_map_item', '⇄ From map item'],
    ].forEach(([value, text]) => {
      const option = h('option', '', text); option.value = value; option.selected = (kind.input?.kind || 'from_trigger') === value; select.append(option);
    });
    const wrap = h('div', 'field'); wrap.append(label, select); body.append(wrap);
    const details = h('div'); body.append(details);
    const renderDetails = () => {
      details.replaceChildren();
      const input = kind.input || { kind: 'from_trigger' };
      if (input.kind === 'literal') {
        const value = h('textarea', 'input'); value.value = input.value || '';
        value.placeholder = kind.type === 'tool' ? 'Tool args as a JSON object' : 'Fixed input';
        value.addEventListener('input', () => { kind.input = { kind: 'literal', value: value.value }; markDirty(); refreshGraphNode(node); });
        details.append(value);
      } else if (input.kind === 'from_upstream') {
        details.append(h('div', 'small muted', 'Outputs from selected nodes are joined with blank lines.'));
        (automation.nodes || []).filter((candidate) => candidate.id !== node.id).forEach((candidate) => {
          const row = h('label', 'check-row');
          const checkbox = h('input'); checkbox.type = 'checkbox'; checkbox.checked = (input.nodes || []).includes(candidate.id);
          checkbox.addEventListener('change', () => {
            const selected = new Set(kind.input?.kind === 'from_upstream' ? kind.input.nodes || [] : []);
            if (checkbox.checked) selected.add(candidate.id); else selected.delete(candidate.id);
            kind.input = { kind: 'from_upstream', nodes: [...selected] };
            markDirty(); refreshGraphNode(node);
          });
          row.append(checkbox, h('span', 'mono', candidate.id));
          details.append(row);
        });
      } else if (input.kind === 'template') {
        const value = h('textarea', 'input'); value.value = input.template || ''; value.placeholder = 'Trigger: {{trigger}}\nPlanner: {{node:planner}}';
        value.addEventListener('input', () => { kind.input = { kind: 'template', template: value.value }; markDirty(); refreshGraphNode(node); });
        details.append(value);
      } else if (input.kind === 'from_map_item') {
        details.append(h('div', 'small muted', 'Uses the current item from a containing Map node.'));
      } else {
        details.append(h('div', 'small muted', 'Uses the trigger input unchanged.'));
      }
    };
    select.addEventListener('change', () => {
      kind.input = select.value === 'literal' ? { kind: 'literal', value: '' }
        : select.value === 'from_upstream' ? { kind: 'from_upstream', nodes: [] }
          : select.value === 'template' ? { kind: 'template', template: '' }
            : select.value === 'from_map_item' ? { kind: 'from_map_item' }
              : { kind: 'from_trigger' };
      markDirty(); renderDetails(); refreshGraphNode(node);
    });
    renderDetails();
  }

  function renderConditionalEditor(body, node) {
    const automation = state.editor.automation;
    const kind = node.kind;
    body.append(h('div', 'divider'), h('div', 'small muted', 'Branches'));
    const list = h('div');
    body.append(list);
    const render = () => {
      list.replaceChildren();
      (kind.branches || []).forEach((branch, index) => {
        const row = h('div', 'branch-row');
        const name = h('input', 'input'); name.value = branch.name || '';
        let previousName = branch.name || '';
        name.addEventListener('input', () => {
          const nextName = name.value;
          if (kind.default === previousName) kind.default = nextName || null;
          (automation.edges || []).filter((edge) => edge.from === node.id && edge.label === previousName)
            .forEach((edge) => { edge.label = nextName || null; });
          branch.name = nextName;
          previousName = nextName;
          markDirty();
          refreshGraphNode(node);
        });
        name.addEventListener('blur', () => renderInspector(node));
        const operator = h('select', 'select');
        [['always', 'always'], ['equals', 'equals'], ['contains', 'contains'], ['matches', 'regex'], ['not_empty', 'not empty']]
          .forEach(([value, label]) => {
            const option = h('option', '', label); option.value = value; option.selected = branch.when?.op === value; operator.append(option);
          });
        operator.addEventListener('change', () => {
          if (operator.value === 'always' || operator.value === 'not_empty') branch.when = { op: operator.value };
          else if (operator.value === 'matches') branch.when = { op: 'matches', pattern: branch.when?.pattern || '' };
          else branch.when = { op: operator.value, value: branch.when?.value || '' };
          markDirty(); render();
        });
        let value = h('span', 'branch-value');
        if (['equals', 'contains', 'matches'].includes(branch.when?.op)) {
          value = h('input', 'input branch-value');
          value.value = branch.when.op === 'matches' ? branch.when.pattern || '' : branch.when.value || '';
          value.placeholder = branch.when.op === 'matches' ? 'regex pattern' : 'value';
          value.addEventListener('input', () => {
            if (branch.when.op === 'matches') branch.when.pattern = value.value;
            else branch.when.value = value.value;
            markDirty();
          });
        }
        const remove = h('button', 'icon-btn', '×'); remove.type = 'button'; remove.title = 'Remove branch';
        remove.setAttribute('aria-label', `Remove branch ${branch.name || index + 1}`);
        remove.addEventListener('click', () => {
          const [removed] = kind.branches.splice(index, 1);
          const removedName = removed?.name || '';
          if (kind.default === removedName) kind.default = null;
          (automation.edges || []).filter((edge) => edge.from === node.id && edge.label === removedName)
            .forEach((edge) => { edge.label = null; });
          markDirty();
          renderInspector(node);
          refreshGraphNode(node);
        });
        row.append(name, operator, value, remove);
        list.append(row);
      });
      const add = h('button', 'btn ghost', '+ Branch'); add.type = 'button';
      add.addEventListener('click', () => {
        kind.branches ||= [];
        kind.branches.push({ name: `branch${kind.branches.length + 1}`, when: { op: 'always' } });
        markDirty(); render(); refreshGraphNode(node);
      });
      list.append(add);
    };
    render();

    const defaultField = h('div', 'field');
    defaultField.append(h('label', '', 'Default branch (no match)'));
    const defaultBranch = h('select', 'select');
    const none = h('option', '', '(no default — halt downstream)'); none.value = ''; defaultBranch.append(none);
    (kind.branches || []).forEach((branch) => {
      const option = h('option', '', branch.name); option.value = branch.name; option.selected = kind.default === branch.name; defaultBranch.append(option);
    });
    defaultBranch.addEventListener('change', () => { kind.default = defaultBranch.value || null; markDirty(); });
    defaultField.append(defaultBranch);
    body.append(defaultField);

    const outgoing = (automation.edges || []).filter((edge) => edge.from === node.id);
    if (outgoing.length) {
      body.append(h('div', 'small muted', 'Outgoing edge branches'));
      outgoing.forEach((edge) => {
        const field = h('div', 'field');
        field.append(h('label', '', `→ ${edge.to}`));
        const control = h('select', 'select');
        const noLabel = h('option', '', '(no label)'); noLabel.value = ''; control.append(noLabel);
        (kind.branches || []).forEach((branch) => {
          const option = h('option', '', branch.name); option.value = branch.name; option.selected = edge.label === branch.name; control.append(option);
        });
        control.addEventListener('change', () => { edge.label = control.value || null; markDirty(); });
        field.append(control);
        body.append(field);
      });
    }
  }

  function renderDeleteNode(body, node) {
    body.append(h('div', 'divider'));
    const remove = h('button', 'btn ghost', '🗑 Delete node');
    remove.type = 'button';
    remove.style.color = 'var(--err, #ff6b6b)';
    remove.addEventListener('click', async () => {
      await deleteAutomationNodes([node.id]);
    });
    body.append(remove);
  }

  async function deleteAutomationNodes(ids) {
    const automation = state.editor.automation;
    const unique = [...new Set(ids)].filter((id) => automation?.nodes?.some((node) => node.id === id));
    if (!automation || !unique.length) return;
    const confirmed = await confirmDialog({
      title: unique.length === 1 ? `Delete node “${unique[0]}”?` : `Delete ${unique.length} nodes?`,
      body: 'Connected edges and typed references will also be removed. Template references are flagged for review.',
      okLabel: 'Delete', danger: true,
    });
    if (!confirmed || automation !== state.editor.automation || state.editor.mode !== 'edit') return;
    const removed = new Set(unique);
    automation.nodes = automation.nodes.filter((candidate) => !removed.has(candidate.id));
    automation.edges = (automation.edges || []).filter((edge) => !removed.has(edge.from) && !removed.has(edge.to));
    let repairedReferences = false;
    const templateReferences = [];
    automation.nodes.forEach((candidate) => {
      const kind = candidate.kind || {};
      if (kind.type === 'map' && removed.has(kind.body_node)) {
        kind.body_node = '';
        repairedReferences = true;
      }
      if (kind.input?.kind === 'from_upstream') {
        const next = (kind.input.nodes || []).filter((id) => !removed.has(id));
        if (next.length !== (kind.input.nodes || []).length) {
          kind.input.nodes = next;
          repairedReferences = true;
        }
      }
      if (kind.input?.kind === 'template') {
        unique.forEach((id) => {
          if (String(kind.input.template || '').includes(`{{node:${id}}}`)) templateReferences.push(id);
        });
      }
    });
    markDirty(); closeInspector(); void renderGraph();
    if (repairedReferences) notify('References updated', 'Typed inputs that pointed to deleted nodes were cleared.', 'info');
    if (templateReferences.length) notify('Review template references', `Templates still mention: ${[...new Set(templateReferences)].join(', ')}.`, 'err');
  }

  async function deleteAutomationEdges(indexes) {
    const automation = state.editor.automation;
    if (!automation) return;
    const selected = new Set(indexes.filter((index) => index >= 0 && index < (automation.edges || []).length));
    if (!selected.size) return;
    const confirmed = await confirmDialog({
      title: selected.size === 1 ? 'Delete this edge?' : `Delete ${selected.size} edges?`,
      body: 'The connected nodes stay in the Automation.', okLabel: 'Delete', danger: true,
    });
    if (!confirmed || automation !== state.editor.automation || state.editor.mode !== 'edit') return;
    automation.edges = (automation.edges || []).filter((_, index) => !selected.has(index));
    markDirty(); void renderGraph();
  }

  function refreshGraphNode(node) {
    const element = byId(`automation-node-${node.id}`);
    if (!element) return;
    const summary = nodeSummary(node);
    const subtitle = element.querySelector('.node-sub');
    const input = element.querySelector('.node-input');
    if (subtitle) subtitle.textContent = summary.subtitle;
    if (input) input.textContent = summary.input;
  }

  async function toggleRuns() {
    if ($('.runs').classList.contains('hide')) {
      $('.runs').classList.remove('hide');
      await refreshRuns();
    } else closeRuns();
  }

  function closeRuns() {
    $('.runs')?.classList.add('hide');
    clearTimeout(state.editor.runsTimer);
    state.editor.runsTimer = 0;
  }

  function scheduleRuns() {
    clearTimeout(state.editor.runsTimer);
    if (!state.connected || $('.runs').classList.contains('hide') || !state.editor.automation) return;
    state.editor.runsTimer = setTimeout(() => void refreshRuns(), 1500);
  }

  async function refreshRuns() {
    clearTimeout(state.editor.runsTimer);
    state.editor.runsTimer = 0;
    const automation = state.editor.automation;
    if (!automation) return;
    const id = automation.id;
    const list = $('.run-list');
    const expanded = new Set(Array.from(list.querySelectorAll('.run.open')).map((card) => card.dataset.runId));
    if (!list.children.length) list.append(h('div', 'small muted', 'Loading…'));
    try {
      const runs = await jsonRequest(`/api/automations/${encodeURIComponent(id)}/runs`, { cache: 'no-store' });
      if (state.editor.automation?.id !== id) return;
      setError('runs', '');
      list.replaceChildren();
      if (!Array.isArray(runs) || !runs.length) list.append(h('div', 'small muted', 'No runs yet. Click Run to start one.'));
      else runs.forEach((run) => list.append(runCard(run, id, expanded.has(run.run_id))));
    } catch (error) {
      if (state.editor.automation?.id !== id) return;
      setError('runs', `Run history could not be loaded: ${error?.message || error}`, () => refreshRuns());
    } finally {
      if (state.editor.automation?.id === id) scheduleRuns();
    }
  }

  function runCard(run, automationId, expanded) {
    const card = h('article', `run${expanded ? ' open' : ''}`);
    card.dataset.runId = run.run_id;
    const head = h('div', 'run-head');
    const left = h('div');
    left.append(h('div', 'mono small', `${String(run.run_id || '').slice(0, 8)} · ${relativeTime(run.started_at_unix)}`));
    if (run.forked_from) {
      const source = String(run.forked_from.source_run_id || '').slice(0, 8);
      left.append(h('div', 'small muted', run.forked_from.from_start
        ? `run again from ${source} · from start`
        : `continued from ${source} @ step ${run.forked_from.from_step}`));
    }
    if (run.status_reason) left.append(h('div', 'run-reason', run.status_reason));
    const actions = h('div', 'run-actions');
    actions.append(h('span', `run-status ${run.status || 'running'}`, run.status || 'running'));
    const rerun = h('button', 'btn ghost', 'Run again');
    rerun.type = 'button';
    rerun.title = 'Start from the beginning with the same trigger input';
    rerun.addEventListener('click', (event) => { event.stopPropagation(); void runAgain(automationId, run.run_id); });
    actions.append(rerun);
    head.append(left, actions);
    card.append(head);
    const steps = h('div', 'run-steps');
    card.append(steps);
    const build = () => {
      if (steps.dataset.built) return;
      steps.dataset.built = 'true';
      if (Object.hasOwn(run, 'final_content')) {
        const result = h('section', 'run-result');
        result.append(h('strong', 'run-result-label', 'Result'));
        result.append(h('pre', 'run-result-content', run.final_content || '(empty result)'));
        steps.append(result);
      } else if (['completed', 'failed'].includes(run.status)) {
        steps.append(h('div', 'run-result-unavailable', 'Result was not recorded for this earlier run.'));
      }
      if (!run.checkpoints?.length) { steps.append(h('div', 'small muted', 'No checkpoints recorded.')); return; }
      run.checkpoints.forEach((checkpoint) => {
        const icon = checkpoint.event === 'node_completed' ? '✓' : checkpoint.event === 'node_failed' ? '✗'
          : checkpoint.event === 'interrupt_parked' ? '⏸' : checkpoint.event === 'interrupt_resumed' ? '▶' : '·';
        const row = h('div', 'run-step');
        row.append(h('span', '', icon), h('span', '', `${checkpoint.node_id} · ${String(checkpoint.event || '').replaceAll('_', ' ')}`));
        if (checkpoint.failure_detail) {
          row.append(h('span'), h('span', 'run-step-detail', checkpoint.failure_detail));
        }
        steps.append(row);
      });
    };
    head.addEventListener('click', () => { card.classList.toggle('open'); if (card.classList.contains('open')) build(); });
    if (expanded) build();
    return card;
  }

  async function runAgain(automationId, runId) {
    const response = await action('Run again failed', () => jsonRequest(
      `/api/automations/${encodeURIComponent(automationId)}/runs/${encodeURIComponent(runId)}/fork`,
      { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' },
    ), { success: 'Automation started again', body: `Reusing the input from ${String(runId).slice(0, 8)}.` });
    if (response != null) setTimeout(() => void refreshRuns(), 350);
  }

  function scheduleInterruptPoll() {
    clearTimeout(state.interruptTimer);
    if (!state.connected) return;
    state.interruptTimer = setTimeout(async () => {
      await refreshInterrupts();
      scheduleInterruptPoll();
    }, 3000);
  }

  async function refreshInterrupts() {
    const generation = ++state.interruptGeneration;
    try {
      const items = await jsonRequest('/api/interrupts', { cache: 'no-store' });
      if (generation !== state.interruptGeneration) return null;
      if (!Array.isArray(items)) throw new Error('The interrupt endpoint returned an invalid list.');
      state.interrupts = items;
      setError('interrupt-load', '');
      const button = $('.interrupt-toggle');
      button.textContent = items.length ? `⏸ ${items.length} waiting` : 'No interrupts';
      button.disabled = items.length === 0;
      const keys = items.map((item) => `${item.automation_id}:${item.run_id}:${item.node_id}`).sort().join('|');
      if (keys !== state.interruptKeys) {
        state.interruptKeys = keys;
        renderInterrupts();
      }
      reconcilePausedNodes();
      emit('interrupts-change', { items: clone(items), count: items.length });
      return items;
    } catch (error) {
      if (generation !== state.interruptGeneration) return null;
      setError('interrupt-load', `Pending interrupts could not be loaded: ${error?.message || error}`, () => refreshInterrupts());
      return null;
    }
  }

  function decodedInterruptText(value) {
    let text = String(value ?? '');
    const trimmed = text.trim();
    if (trimmed.startsWith('"') && trimmed.endsWith('"')) {
      try {
        const decoded = JSON.parse(trimmed);
        if (typeof decoded === 'string') text = decoded;
      } catch {}
    }
    return text
      .replaceAll('\\r\\n', '\n')
      .replaceAll('\\n', '\n')
      .replaceAll('\\r', '\n')
      .replaceAll('\\t', '\t')
      .replaceAll('\r\n', '\n');
  }

  function appendInterruptInline(parent, value) {
    const text = String(value || '');
    const token = /(\*\*[^*\n]+\*\*|`[^`\n]+`|\[[^\]\n]+\]\(https?:\/\/[^)\s]+\))/g;
    let cursor = 0;
    for (const match of text.matchAll(token)) {
      if (match.index > cursor) parent.append(document.createTextNode(text.slice(cursor, match.index)));
      const valueText = match[0];
      if (valueText.startsWith('**')) {
        parent.append(h('strong', '', valueText.slice(2, -2)));
      } else if (valueText.startsWith('`')) {
        parent.append(h('code', '', valueText.slice(1, -1)));
      } else {
        const parts = valueText.match(/^\[([^\]]+)\]\((https?:\/\/[^)]+)\)$/);
        const link = h('a', '', parts?.[1] || valueText);
        link.href = parts?.[2] || '#'; link.target = '_blank'; link.rel = 'noopener noreferrer';
        parent.append(link);
      }
      cursor = match.index + valueText.length;
    }
    if (cursor < text.length) parent.append(document.createTextNode(text.slice(cursor)));
  }

  function appendInterruptRichText(parent, value) {
    const text = decodedInterruptText(value);
    const lines = text.split('\n');
    let list = null;
    for (let index = 0; index < lines.length; index += 1) {
      const line = lines[index];
      if (line.trim().startsWith('```')) {
        const codeLines = [];
        index += 1;
        while (index < lines.length && !lines[index].trim().startsWith('```')) {
          codeLines.push(lines[index]); index += 1;
        }
        const pre = h('pre'); pre.append(h('code', '', codeLines.join('\n'))); parent.append(pre); list = null; continue;
      }
      if (!line.trim()) { list = null; continue; }
      const heading = line.match(/^\s*(#{1,6})\s+(.+)$/);
      if (heading) {
        const level = heading[1].length <= 2 ? 'h3' : 'h4';
        const node = h(level); appendInterruptInline(node, heading[2]); parent.append(node); list = null; continue;
      }
      const bullet = line.match(/^\s*[-*]\s+(.+)$/);
      const ordered = line.match(/^\s*\d+[.)]\s+(.+)$/);
      if (bullet || ordered) {
        const tag = ordered ? 'ol' : 'ul';
        if (!list || list.localName !== tag) { list = h(tag); parent.append(list); }
        const item = h('li'); appendInterruptInline(item, (bullet || ordered)[1]); list.append(item); continue;
      }
      const quote = line.match(/^\s*>\s?(.*)$/);
      const node = h(quote ? 'blockquote' : 'p');
      appendInterruptInline(node, quote ? quote[1] : line); parent.append(node); list = null;
    }
  }

  function appendInterruptBody(parent, value) {
    let source = String(value ?? '') || '(no message)';
    const trimmed = source.trim();
    if (trimmed.startsWith('"') && trimmed.endsWith('"')) {
      try {
        const decoded = JSON.parse(trimmed);
        if (typeof decoded === 'string') source = decoded;
      } catch {}
    }
    const marker = source.match(/(?:^|\r?\n|\\r?\\n)Reviews:\s*(?:(?:\r?\n)|(?:\\r?\\n))?/i);
    if (!marker) { appendInterruptRichText(parent, source); return; }
    const lead = source.slice(0, marker.index);
    const encodedReviews = source.slice(marker.index + marker[0].length).trim();
    let reviews = null;
    try {
      const parsed = JSON.parse(encodedReviews);
      if (Array.isArray(parsed)) reviews = parsed;
    } catch {}
    if (!reviews) { appendInterruptRichText(parent, source); return; }
    if (lead.trim()) appendInterruptRichText(parent, lead);
    const section = h('section', 'interrupt-reviews');
    section.append(h('h3', '', `Reviews (${reviews.length})`));
    const list = h('ol', 'interrupt-review-list');
    reviews.forEach((review) => {
      const item = h('li', 'interrupt-review');
      appendInterruptRichText(item, typeof review === 'string' ? review : JSON.stringify(review, null, 2));
      list.append(item);
    });
    section.append(list); parent.append(section);
  }

  function renderInterrupts() {
    const list = $('.interrupt-list');
    if (!list) return;
    const drafts = new Map(Array.from(list.querySelectorAll('.interrupt')).map((card) => [card.dataset.key, card.querySelector('textarea')?.value || '']));
    list.replaceChildren();
    if (!state.interrupts.length) {
      list.append(h('div', 'empty', 'No pending interrupts.'));
      return;
    }
    state.interrupts.forEach((item) => {
      const key = `${item.automation_id}:${item.run_id}:${item.node_id}`;
      const card = h('article', 'interrupt');
      card.dataset.key = key;
      card.append(h('div', 'meta mono', `${item.automation_id} · run ${String(item.run_id || '').slice(0, 8)} · node ${item.node_id}`));
      const message = h('div', 'interrupt-message');
      appendInterruptBody(message, item.message || '(no message)');
      card.append(message);
      const actions = h('div', 'interrupt-actions');
      const guidance = h('textarea', 'input');
      guidance.rows = 2;
      guidance.value = drafts.get(key) || '';
      guidance.placeholder = 'Your guidance — becomes the node output. Cmd/Ctrl+Enter submits.';
      guidance.setAttribute('aria-label', `Resume guidance for ${item.automation_id}, node ${item.node_id}`);
      const resume = h('button', 'btn', '✓ Resume'); resume.type = 'button';
      resume.setAttribute('aria-label', `Resume ${item.automation_id}, node ${item.node_id}`);
      const cancel = h('button', 'btn ghost', 'Skip'); cancel.type = 'button';
      cancel.setAttribute('aria-label', `Skip ${item.automation_id}, node ${item.node_id}`);
      const submit = () => void resumeInterrupt(item, guidance.value || '', resume, cancel);
      guidance.addEventListener('keydown', (event) => {
        if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) { event.preventDefault(); submit(); }
      });
      resume.addEventListener('click', submit);
      cancel.addEventListener('click', () => void cancelInterrupt(item, resume, cancel));
      actions.append(guidance, resume, cancel);
      card.append(actions);
      list.append(card);
    });
  }

  function reconcilePausedNodes() {
    const automationId = state.editor.automation?.id;
    if (!automationId) return;
    const paused = new Set(state.interrupts
      .filter((item) => item.automation_id === automationId)
      .map((item) => item.node_id));
    byId('automation-settings-lattice')?.querySelectorAll('ax-node[id^="automation-node-"]').forEach((node) => {
      node.classList.toggle('paused', paused.has(node.id.slice('automation-node-'.length)));
    });
  }

  async function resumeInterrupt(item, value, ...buttons) {
    buttons.forEach((button) => { button.disabled = true; });
    try {
      await jsonRequest(
        `/api/automations/${encodeURIComponent(item.automation_id)}/runs/${encodeURIComponent(item.run_id)}/nodes/${encodeURIComponent(item.node_id)}/resume`,
        { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ value }) },
      );
      setError('interrupt-action', '');
      notify('Interrupt resumed', item.node_id, 'ok');
      await refreshInterrupts();
      if (!$('.runs').classList.contains('hide') && state.editor.automation?.id === item.automation_id) void refreshRuns();
    } catch (error) {
      setError('interrupt-action', `Resume failed for ${item.node_id}: ${error?.message || error}`, () => resumeInterrupt(item, value, ...buttons));
      notify('Resume failed', String(error?.message || error), 'err');
      buttons.forEach((button) => { button.disabled = false; });
    }
  }

  async function cancelInterrupt(item, ...buttons) {
    const confirmed = await confirmDialog({
      title: `Skip interrupt “${item.node_id}”?`,
      body: 'This node will continue with an empty output. The Automation run itself will not be cancelled.', okLabel: 'Skip with empty output', danger: true,
    });
    if (!confirmed) return;
    buttons.forEach((button) => { button.disabled = true; });
    try {
      await jsonRequest(
        `/api/automations/${encodeURIComponent(item.automation_id)}/runs/${encodeURIComponent(item.run_id)}/nodes/${encodeURIComponent(item.node_id)}/cancel`,
        { method: 'POST' },
      );
      setError('interrupt-action', '');
      notify('Interrupt skipped', `${item.node_id} continued with empty output`, 'ok');
      await refreshInterrupts();
      if (!$('.runs').classList.contains('hide') && state.editor.automation?.id === item.automation_id) void refreshRuns();
    } catch (error) {
      setError('interrupt-action', `Skip failed for ${item.node_id}: ${error?.message || error}`, () => cancelInterrupt(item, ...buttons));
      notify('Skip failed', String(error?.message || error), 'err');
      buttons.forEach((button) => { button.disabled = false; });
    }
  }

  function handleFrame(frame) {
    if (!frame || typeof frame !== 'object') return;
    const eventType = frame.event_type || frame.type || '';
    if (eventType === 'Interrupted' || eventType === 'Resumed' || eventType === 'Cancelled') void refreshInterrupts();
    const automationId = frame.workflow || frame.automation_id;
    const nodeId = frame.task || frame.node_id;
    if (automationId && nodeId && state.editor.automation?.id === automationId) {
      const status = eventType === 'Interrupted' ? 'paused'
        : ['AgentFailed', 'TaskFailed', 'NodeFailed'].includes(eventType) ? 'failed'
          : ['TaskCompleted', 'MapCompleted', 'Resumed', 'NodeCompleted', 'Branched', 'Cancelled'].includes(eventType) ? 'complete' : 'live';
      pulseNode(nodeId, status);
    }
    if (!$('.runs').classList.contains('hide') && automationId === state.editor.automation?.id) {
      clearTimeout(state.frameTimer);
      state.frameTimer = setTimeout(() => void refreshRuns(), 180);
    }
  }

  function pulseNode(nodeId, status) {
    const node = byId(`automation-node-${nodeId}`);
    if (!node) return;
    node.classList.remove('live', 'complete', 'paused', 'failed');
    node.classList.add(status);
    if (status === 'complete' || status === 'failed') {
      setTimeout(() => node.classList.remove(status), 3000);
    }
  }
}

if (!customElements.get('ax-automation-settings')) {
  customElements.define('ax-automation-settings', AxAutomationSettings);
}
