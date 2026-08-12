/** Shared, buildless helpers for the Settings feature components. */

export const h = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
};

export const formatNumber = (value) => new Intl.NumberFormat().format(Number(value) || 0);

export const teamClass = (team) => ({
  Engineering: 'eng', Research: 'res', Ops: 'ops', Customer: 'cust',
}[team] || 'general');

export async function jsonRequest(url, options) {
  let response;
  try {
    response = await fetch(url, options);
  } catch (error) {
    throw new Error(`Could not reach Axocoatl: ${error?.message || error}`);
  }
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

export function emit(host, type, detail) {
  host.dispatchEvent(new CustomEvent(type, {
    detail, bubbles: true, composed: true,
  }));
}

export const SETTINGS_CSS = `
:host {
  display: flex; flex: 1; min-width: 0; min-height: 0; height: 100%;
  color: var(--text); background: var(--panel); font-family: var(--font-sans);
}
* { box-sizing: border-box; }
button, input, select, textarea { font: inherit; }
button { color: inherit; }
.shell {
  display: grid; grid-template-columns: 240px minmax(0, 1fr);
  flex: 1; min-height: 0; overflow: hidden; background: var(--panel);
}
.side {
  display: flex; min-width: 0; min-height: 0; flex-direction: column;
  overflow: hidden; border-right: 1px solid var(--border); background: var(--bg-2);
}
.side-search { padding: var(--sp-2); flex-shrink: 0; }
.side-scroll { flex: 1; min-height: 0; overflow: auto; padding-bottom: var(--sp-2); }
.side-section { padding: var(--sp-1) 0; }
.side-head {
  display: flex; width: 100%; align-items: center; gap: var(--sp-1);
  padding: 7px 10px 4px; border: 0; background: transparent;
  color: var(--muted); cursor: pointer; text-align: left;
  font-size: var(--fs-xs); font-weight: var(--fw-medium);
  letter-spacing: .07em; text-transform: uppercase;
}
.side-head .tri { width: 10px; color: var(--muted-2); transition: transform var(--dur-fast) var(--ease); }
.side-section[data-collapsed] .tri { transform: rotate(-90deg); }
.side-section[data-collapsed] .side-list { display: none; }
.side-list { display: grid; gap: 1px; padding: 0 6px; }
.side-row {
  display: flex; width: 100%; align-items: center; gap: var(--sp-2);
  min-width: 0; padding: 5px 9px; border: 0; border-radius: var(--r-md);
  background: transparent; color: var(--text); cursor: pointer; text-align: left;
  font-size: var(--fs-sm);
}
.side-row:hover { background: var(--bg-3); }
.side-row[aria-current="true"] { background: rgba(var(--axo-jade-rgb), .22); }
.side-row .ico { width: 14px; flex-shrink: 0; text-align: center; }
.side-row .label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.side-row .count { flex-shrink: 0; color: var(--muted-2); font: var(--fs-xs) var(--font-mono); }
.dot { width: 7px; height: 7px; flex-shrink: 0; border-radius: 50%; background: var(--muted-2); }
.dot.on { background: var(--ok); box-shadow: 0 0 6px var(--ok); }
.dot.run { background: var(--axo-jade-glow); box-shadow: 0 0 6px var(--axo-jade-glow); animation: pulse 1.6s infinite; }
.dot.err { background: var(--err); }
@keyframes pulse { 50% { opacity: .35; } }
.side-empty, .empty { padding: var(--sp-3); color: var(--muted-2); font-size: var(--fs-xs); line-height: var(--lh-body); }
.main { display: flex; position: relative; min-width: 0; min-height: 0; flex-direction: column; overflow: hidden; }
.toolbar {
  display: flex; min-height: 40px; align-items: center; gap: var(--sp-2);
  padding: 7px var(--sp-3); border-bottom: 1px solid var(--border);
  background: var(--panel-2); flex-shrink: 0;
}
.toolbar h2 { margin: 0; font-size: var(--fs-body); font-weight: var(--fw-medium); }
.toolbar .sub { color: var(--muted); font-size: var(--fs-xs); }
.grow { flex: 1; }
.content { flex: 1; min-height: 0; overflow: auto; }
.search, .field, select, textarea {
  min-width: 0; padding: 5px var(--sp-2); border: 1px solid var(--border);
  border-radius: var(--r-sm); color: var(--text); background: var(--bg-2);
  font-size: var(--fs-xs);
}
.search { width: 100%; }
.toolbar .search { width: 200px; }
textarea { width: 100%; resize: vertical; line-height: var(--lh-body); }
button:focus-visible, input:focus-visible, select:focus-visible, textarea:focus-visible {
  outline: none; box-shadow: var(--focus-ring);
}
.action {
  padding: 5px var(--sp-3); border: 1px solid var(--border-strong);
  border-radius: var(--r-md); background: var(--bg-3); color: var(--text);
  cursor: pointer; font-size: var(--fs-xs); font-weight: var(--fw-medium);
}
.action:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
.action.primary { background: var(--accent); border-color: var(--accent); color: var(--bg); }
.action.danger:hover:not(:disabled) { border-color: var(--err); color: var(--err); }
.action:disabled { opacity: .5; cursor: default; }
.icon-button { padding: 2px 6px; border: 0; background: transparent; color: var(--muted); cursor: pointer; font-size: var(--fs-lg); }
.icon-button:hover { color: var(--text); }
.errors:empty { display: none; }
.error {
  display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: var(--sp-1) var(--sp-2);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--border);
  color: var(--err); background: var(--bg-3); font-size: var(--fs-xs);
}
.error strong { font-weight: var(--fw-medium); }
.error span { grid-column: 1 / -1; overflow-wrap: anywhere; white-space: pre-wrap; }
.error button { border: 0; background: transparent; color: var(--accent-2); cursor: pointer; }
.loading { opacity: .65; }
.badge {
  display: inline-flex; align-items: center; width: fit-content;
  padding: 2px 7px; border-radius: var(--r-pill); background: var(--bg-3);
  color: var(--muted); font-size: var(--fs-xs);
}
.badge.idle { color: var(--muted); }
.badge.running { color: var(--ok); background: rgba(var(--axo-jade-rgb), .16); }
.badge.failed { color: var(--err); background: rgba(226, 106, 106, .13); }
.team.eng { color: var(--team-eng); background: rgba(var(--axo-jade-rgb), .15); }
.team.res { color: var(--team-res); background: rgba(var(--axo-blue-rgb), .15); }
.team.ops { color: var(--team-ops); background: rgba(var(--axo-bronze-rgb), .15); }
.team.cust { color: var(--team-cust); background: rgba(var(--axo-jade-rgb), .15); }
.team.general { color: var(--muted); }
.team-dot { width: 9px; height: 9px; flex-shrink: 0; border-radius: 50%; background: var(--muted-2); }
.team-dot.eng { background: var(--team-eng); } .team-dot.res { background: var(--team-res); }
.team-dot.ops { background: var(--team-ops); } .team-dot.cust { background: var(--team-cust); }
.mono { font-family: var(--font-mono); }
.muted { color: var(--muted); }
.drawer {
  position: absolute; z-index: 5; inset: 0 0 0 auto; display: flex;
  width: min(440px, 92%); min-height: 0; flex-direction: column;
  border-left: 1px solid var(--border); background: var(--panel);
  box-shadow: -8px 0 24px rgba(0,0,0,.25);
  transform: translateX(101%); transition: transform var(--dur-base) var(--ease);
}
.drawer[data-open] { transform: translateX(0); }
.drawer-head, .drawer-foot {
  display: flex; flex-shrink: 0; align-items: center; gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--border);
  background: var(--panel-2);
}
.drawer-head h3 { flex: 1; min-width: 0; margin: 0; overflow: hidden; text-overflow: ellipsis; font-size: var(--fs-body); }
.drawer-body { flex: 1; min-height: 0; overflow: auto; padding: var(--sp-3); }
.drawer-foot { justify-content: flex-end; border-top: 1px solid var(--border); border-bottom: 0; }
.unsaved { margin-right: auto; color: var(--warn); font-size: var(--fs-xs); }
.section { margin-bottom: var(--sp-4); }
.section h4 { margin: 0 0 var(--sp-2); font-size: var(--fs-xs); letter-spacing: .06em; text-transform: uppercase; color: var(--muted); }
.field-row { display: grid; grid-template-columns: 112px minmax(0, 1fr); align-items: center; gap: var(--sp-2); margin-bottom: var(--sp-2); }
.field-row > label, .field-label { color: var(--muted); font-size: var(--fs-xs); }
.row { display: flex; align-items: center; gap: var(--sp-2); }
.card { border: 1px solid var(--border); border-radius: var(--r-lg); background: var(--bg-2); overflow: hidden; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm); }
th, td { padding: 7px var(--sp-3); border-bottom: 1px solid var(--border); text-align: left; vertical-align: middle; }
th { position: sticky; z-index: 1; top: 0; background: var(--panel-2); color: var(--muted); font-size: var(--fs-xs); font-weight: var(--fw-medium); }
tbody tr:last-child td { border-bottom: 0; }
tbody tr.clickable { cursor: pointer; }
tbody tr.clickable:hover { background: var(--bg-2); }
@media (max-width: 760px) {
  .shell { grid-template-columns: 180px minmax(0, 1fr); }
  .toolbar .search { width: 125px; }
  th, td { padding-inline: var(--sp-2); }
}
@media (max-width: 560px) {
  .shell { grid-template-columns: 1fr; }
  .side { max-height: 180px; border-right: 0; border-bottom: 1px solid var(--border); }
  .toolbar { flex-wrap: wrap; }
  .toolbar .search { width: 100%; flex-basis: 100%; }
}
`;
