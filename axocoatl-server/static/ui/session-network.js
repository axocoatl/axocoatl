import { adopt } from './sheets.js';

/**
 * `<ax-session-network>`: the Session's network mode, egress policy and
 * network record, with each request on an egress route and how it ended. A
 * person can allow a refused host for this Session, and approve or reject
 * the hosts Agents asked for with `request_network_access`.
 *
 * `open({sessionId})` shows the dialog and reads
 * `GET /api/sessions/{id}/network`. "Allow for this Session" posts
 * `/network/allow` with the refused row's scope and a fresh command id;
 * "Approve" and "Reject" post `/network/proposals/{proposal_id}/approve` or
 * `/reject`. A lost answer is resent with the same id, and a 409 for it means
 * the first one applied.
 */

const PAGE = 1000;
const MAX_PAGES = 20;

const REASONS = {
  not_allowed: 'Not in the allowlist.',
  private_destination: 'Resolves to a private address that is not listed under private_destinations.',
  forbidden_destination: 'Resolves to a loopback, link-local, host-gateway or other special address, which is never allowed.',
  no_credential: 'The process had no egress credential: a read-only helper, a check or a command started outside a tool call.',
  unknown_credential: 'The credential had already ended with its tool call, setup step or terminal.',
  binding_ended: 'The terminal or tool call the credential belonged to had ended.',
  record_unavailable: 'The network record was full or unavailable, so new connections were refused.',
  invalid_host: 'Not a valid host name or IP address.',
  resolve_failed: 'The name did not resolve on this computer.',
  tls_required: 'An egress route: Axocoatl reads its requests only over HTTPS.',
  route_not_for_binding: 'An egress route that does not serve this kind of process.',
};

/** Why a route refused one request. */
const REQUEST_REASONS = {
  route_denied: 'No rule of the route allows this request.',
  host_mismatch: 'The Host header named another site than the route\'s host.',
  path_not_canonical: 'The path was not in canonical form.',
  upgrade_not_allowed: 'Routes forward HTTP/1.1 requests only.',
  override_header: 'A header named another method, path or host.',
  route_not_for_binding: 'The route does not serve this kind of process.',
  request_too_large: 'The body was larger than the route allows.',
  credential_unavailable: 'The daemon could not read the route\'s credential.',
  record_unavailable: 'The request could not be recorded, so it was not sent.',
};

/** Scopes a person can widen for one Session. Provisioning's is fixed. */
const ALLOWABLE_SCOPES = new Set(['session', 'browser']);

const CSS = `
:host { color: var(--text); }
* { box-sizing: border-box; }
dialog { width: min(52rem, 94vw); max-height: 90dvh; overflow: auto; background: var(--panel); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-lg, 10px); padding: var(--sp-5, 20px); }
dialog::backdrop { background: rgba(0,0,0,.55); }
header { display: flex; justify-content: space-between; align-items: start; gap: var(--sp-3, 12px); }
h2 { margin: 0; font-size: var(--fs-title, 16px); }
h3 { font-size: var(--fs-sm, 12.5px); margin: 20px 0 8px; }
p { margin: 6px 0; }
button { font: inherit; color: inherit; background: var(--bg-3); border: 1px solid var(--border-strong, var(--border));
  border-radius: var(--r-md, 6px); padding: 5px 8px; cursor: pointer; }
button:disabled { opacity: .5; cursor: default; }
button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
.muted { color: var(--muted); font-size: var(--fs-xs, 11px); }
.banner { border: 1px solid var(--warn); color: var(--warn); border-radius: var(--r-md, 6px); padding: 8px; margin: 10px 0; }
.counts { display: flex; flex-wrap: wrap; gap: 6px 18px; }
ul { margin: 0; padding-left: 18px; }
li { margin: 3px 0; overflow-wrap: anywhere; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm, 12.5px); }
th, td { text-align: left; vertical-align: top; padding: 6px 8px 6px 0; border-top: 1px solid var(--border); overflow-wrap: anywhere; }
th { color: var(--muted); font-weight: 500; }
code { font-family: var(--font-mono, monospace); font-size: var(--fs-xs, 11px); }
@media (max-width: 560px) { thead { display: none; } tr { display: grid; padding: 6px 0; border-top: 1px solid var(--border); }
  td { border: 0; padding: 2px 0; } }
`;

function element(tag, className = '', text = '') {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function isIpLiteral(host) {
  return /^\d{1,3}(\.\d{1,3}){3}$/.test(host) || host.includes(':') || host.startsWith('[');
}

function when(ms) {
  return typeof ms === 'number' ? new Date(ms).toLocaleString() : '';
}

function who(binding) {
  if (!binding) return 'no credential';
  if (binding.agent) return binding.agent;
  if (binding.kind === 'terminal') return `terminal ${binding.terminal_id || ''}`.trim();
  if (binding.kind === 'setup') return 'setup';
  return binding.kind || 'unknown';
}

function bytes(count) {
  if (count >= 1024 * 1024) return `${(count / (1024 * 1024)).toFixed(1)} MB`;
  if (count >= 1024) return `${(count / 1024).toFixed(1)} KB`;
  return `${count} B`;
}

/** Fold network record lines into what the panel shows. Exported for tests. */
export function summarizeNetwork(lines) {
  const summary = { allowed: 0, refused: 0, bytesIn: 0, bytesOut: 0, refusedRows: [], web: [], requests: [] };
  const owners = new Map();
  const responses = new Map();
  for (const line of lines) {
    const event = line?.event;
    if (!event) continue;
    if (event.kind === 'open') owners.set(event.conn, who(event.binding));
    if (event.kind === 'request') summary.requests.push({ seq: line.seq, at: line.ts_ms, ...event });
    if (event.kind === 'response') responses.set(`${event.conn}#${event.seq_in_conn}`, event);
    if (event.kind === 'open' && event.decision === 'allow') summary.allowed += 1;
    if (event.kind === 'open' && event.decision === 'deny') {
      summary.refused += 1;
      summary.refusedRows.push({
        seq: line.seq, at: line.ts_ms, who: who(event.binding), host: event.host, port: event.port,
        reason: event.reason || 'refused', scope: event.scope || null,
      });
    }
    if (event.kind === 'close') { summary.bytesIn += event.down || 0; summary.bytesOut += event.up || 0; }
    if (event.kind === 'web') summary.web.push({ seq: line.seq, at: line.ts_ms, ...event });
  }
  summary.refusedRows.reverse();
  summary.requests = summary.requests
    .map((request) => ({
      ...request,
      who: owners.get(request.conn) || 'unknown',
      response: responses.get(`${request.conn}#${request.seq_in_conn}`) || null,
    }))
    .reverse();
  return summary;
}

class AxSessionNetwork extends HTMLElement {
  #dialog;
  #body;
  #status;
  #model = null;
  #view = null;
  #lines = [];
  #busy = false;
  #pending = null;

  constructor() {
    super();
    const root = this.attachShadow({ mode: 'open' });
    void adopt(root, CSS, ['/ui/tokens.css']);
    this.#dialog = element('dialog');
    this.#dialog.setAttribute('aria-label', 'Session network');
    const header = element('header');
    header.append(element('h2', '', 'Session network'));
    const close = element('button', 'close', 'Close');
    close.onclick = () => this.close();
    header.append(close);
    const refresh = element('button', 'refresh', 'Refresh network');
    refresh.onclick = () => void this.load();
    this.#body = element('section', 'body');
    this.#status = element('p', 'muted');
    this.#status.setAttribute('role', 'status');
    this.#dialog.append(header, refresh, this.#body, this.#status);
    this.#dialog.addEventListener('keydown', (event) => { if (event.key === 'Escape') event.stopPropagation(); });
    root.append(this.#dialog);
  }

  async open(model) {
    this.#model = model;
    this.#pending = null;
    this.#dialog.showModal();
    await this.load();
  }

  close() { this.#dialog.close(); }

  #url(path = '') {
    return `/api/sessions/${encodeURIComponent(this.#model.sessionId)}/network${path}`;
  }

  async load() {
    if (this.#busy || !this.#model) return;
    this.#busy = true;
    try {
      let after = null;
      const lines = [];
      let view = null;
      for (let page = 0; page < MAX_PAGES; page += 1) {
        const query = new URLSearchParams({ limit: String(PAGE) });
        if (after !== null) query.set('after', String(after));
        const response = await fetch(`${this.#url()}?${query}`);
        const result = await response.json();
        if (!response.ok) throw new Error(result.error || `Network request failed (${response.status})`);
        view = result;
        lines.push(...result.events);
        if (result.events.length < PAGE || result.next_after == null) break;
        after = result.next_after;
      }
      this.#view = view;
      this.#lines = lines;
      this.#status.textContent = '';
    } catch (error) {
      this.#status.textContent = error.message;
    } finally {
      this.#busy = false;
    }
    this.#render();
  }

  #render() {
    const view = this.#view;
    const body = this.#body;
    body.replaceChildren();
    if (!view) return;
    const egress = view.mode === 'egress';
    const mode = element('p');
    mode.textContent = egress
      ? 'Network: egress. Session containers reach only the hosts listed below, through Axocoatl\'s egress proxy.'
      : `Network: ${view.mode}. ${view.mode === 'none' ? 'Session containers have no network.' : 'Session containers can make outbound connections, and nothing is recorded per connection.'}`;
    body.append(mode);
    if (egress) {
      body.append(element('p', 'muted', 'Per-Agent host limits are not enforced inside one Session container: Agents in this Session share its credentials\' reach.'));
      const sidecar = view.sidecar;
      body.append(element('p', 'muted', sidecar
        ? `Egress proxy: ${sidecar.state} (generation ${sidecar.generation}, ${sidecar.restarts} restart${sidecar.restarts === 1 ? '' : 's'}).`
        : 'Egress proxy: not running. It starts with the Session\'s runtime.'));
      if (sidecar?.state === 'failed') {
        body.append(element('p', 'banner', 'The egress proxy stopped after repeated failures. Connections are refused until the Session\'s runtime starts again.'));
      }
    }
    if (view.record?.full) {
      body.append(element('p', 'banner', 'The network record is full, so new connections are refused.'));
    }
    for (const warning of view.warnings || []) body.append(element('p', 'banner warning', warning));
    const proposals = view.proposals || [];
    const pending = proposals.filter((proposal) => proposal.state === 'pending');
    if (pending.length) {
      body.append(element('p', 'banner proposals-waiting', `${pending.length} host request${pending.length === 1 ? '' : 's'} from Agents wait${pending.length === 1 ? 's' : ''} for you below.`));
    }
    const summary = summarizeNetwork(this.#lines);
    const counts = element('div', 'counts');
    counts.append(
      element('span', 'allowed-count', `${summary.allowed} allowed`),
      element('span', 'refused-count', `${summary.refused} refused`),
      element('span', '', `${bytes(summary.bytesIn)} in · ${bytes(summary.bytesOut)} out`),
      element('span', 'muted', `${view.record?.events ?? 0} recorded events`),
    );
    body.append(counts);
    if (view.record?.gaps) body.append(element('p', 'banner', `The record skips ${view.record.gaps} sequence number(s).`));

    for (const policy of view.policies || []) {
      body.append(element('h3', '', `${policy.scope === 'session' ? 'Allowed for this Session' : policy.scope === 'provisioning' ? 'Allowed while provisioning' : 'Allowed for the browser'} · revision ${policy.revision}`));
      const list = element('ul');
      list.dataset.scope = policy.scope;
      if (!policy.rules.length) list.append(element('li', 'muted', 'Nothing.'));
      for (const rule of policy.rules) list.append(element('li', rule.source === 'session' ? 'session-rule' : '', rule.text));
      body.append(list);
    }

    if (proposals.length) {
      body.append(element('h3', '', 'Hosts Agents asked for'));
      body.append(element('p', 'muted', 'Only you can approve a request. Approving allows the host for this Session, like Allow for this Session; the Agent\'s waiting call then continues.'));
      const table = element('table', 'proposals');
      const head = element('thead');
      const headRow = element('tr');
      for (const label of ['Agent', 'Destination', 'Reason', '']) headRow.append(element('th', '', label));
      head.append(headRow);
      const rows = element('tbody');
      for (const proposal of proposals) {
        const tr = element('tr');
        tr.dataset.proposal = proposal.id;
        tr.dataset.state = proposal.state;
        const destination = `${proposal.host}:${(proposal.ports || []).join(',')}`;
        tr.append(element('td', '', proposal.agent || 'unknown'), element('td', '', destination));
        tr.append(element('td', 'reason', proposal.reason || ''));
        const action = element('td');
        if (proposal.state === 'pending') {
          const approve = element('button', 'approve', 'Approve');
          approve.setAttribute('aria-label', `Approve ${destination} for this Session`);
          approve.disabled = this.#busy;
          approve.onclick = () => void this.#decide(proposal, 'approve');
          const reject = element('button', 'reject', 'Reject');
          reject.setAttribute('aria-label', `Reject ${destination}`);
          reject.disabled = this.#busy;
          reject.onclick = () => void this.#decide(proposal, 'reject');
          action.append(approve, ' ', reject);
        } else {
          action.append(element('span', `muted decided ${proposal.state}`, proposal.state === 'approved'
            ? `Approved${proposal.revision ? ` · revision ${proposal.revision}` : ''}`
            : 'Rejected'));
        }
        tr.append(action);
        rows.append(tr);
      }
      table.append(head, rows);
      body.append(table);
    }

    body.append(element('h3', '', 'Refused connections'));
    if (!summary.refusedRows.length) {
      body.append(element('p', 'muted', 'None recorded.'));
    } else {
      const table = element('table', 'refused');
      const head = element('thead');
      const headRow = element('tr');
      for (const label of ['Time', 'Agent', 'Destination', 'Reason', '']) headRow.append(element('th', '', label));
      head.append(headRow);
      const rows = element('tbody');
      for (const row of summary.refusedRows) {
        const tr = element('tr');
        tr.dataset.seq = String(row.seq);
        tr.append(element('td', 'muted', when(row.at)), element('td', '', row.who),
          element('td', '', `${row.host}:${row.port}`));
        const reason = element('td');
        reason.append(element('code', '', row.reason), element('div', 'muted', REASONS[row.reason] || ''));
        tr.append(reason);
        const action = element('td');
        if (egress && row.reason === 'not_allowed' && row.scope === 'provisioning') {
          action.append(element('span', 'muted provisioning-note', 'Provisioning reaches only the distribution mirrors of its presets; it cannot be widened for one Session.'));
        } else if (egress && row.reason === 'not_allowed' && ALLOWABLE_SCOPES.has(row.scope) && !isIpLiteral(row.host)) {
          const allow = element('button', 'allow', 'Allow for this Session');
          allow.setAttribute('aria-label', `Allow ${row.host}:${row.port} for this Session`);
          allow.disabled = this.#busy;
          allow.onclick = () => void this.#allow(row);
          action.append(allow);
        }
        tr.append(action);
        rows.append(tr);
      }
      table.append(head, rows);
      body.append(table);
    }

    if (summary.requests.length) {
      body.append(element('h3', '', 'Requests on routes'));
      body.append(element('p', 'muted', 'Axocoatl ends TLS for these hosts on this computer, checks each request against the route\'s rules and adds the route\'s credential. Each request is recorded before it is sent.'));
      const table = element('table', 'requests');
      const head = element('thead');
      const headRow = element('tr');
      for (const label of ['Time', 'Agent', 'Request', 'Result']) headRow.append(element('th', '', label));
      head.append(headRow);
      const rows = element('tbody');
      for (const request of summary.requests) {
        const tr = element('tr');
        tr.dataset.seq = String(request.seq);
        tr.dataset.decision = request.decision;
        tr.append(element('td', 'muted', when(request.at)), element('td', '', request.who),
          element('td', '', `${request.method} ${request.host}${request.path}`));
        const result = element('td');
        if (request.decision === 'allow') {
          const response = request.response;
          const parts = [request.rule || 'allowed'];
          parts.push(response ? `${response.status} (${response.outcome})` : 'no response recorded');
          if (request.credential) parts.push(`credential ${request.credential}`);
          result.append(element('span', '', parts.join(' · ')));
        } else {
          result.append(element('code', '', request.reason || 'refused'),
            element('div', 'muted', REQUEST_REASONS[request.reason] || ''));
        }
        tr.append(result);
        rows.append(tr);
      }
      table.append(head, rows);
      body.append(table);
    }

    if (summary.web.length) {
      body.append(element('h3', '', 'Web searches and fetches'));
      const list = element('ul', 'web');
      for (const entry of summary.web.slice().reverse()) {
        const item = element('li', '', `${entry.tool} · ${entry.decision}${entry.url ? ` · ${entry.url}` : ''}`);
        if (entry.source_ids?.length) item.append(' ', element('code', '', entry.source_ids.join(' ')));
        list.append(item);
      }
      body.append(list);
    }
  }

  async #decide(proposal, action) {
    if (this.#busy) return;
    const destination = `${proposal.host}:${(proposal.ports || []).join(',')}`;
    const pending = this.#pending?.proposal === proposal.id && this.#pending?.action === action
      ? this.#pending
      : { proposal: proposal.id, action, command_id: crypto.randomUUID() };
    this.#pending = pending;
    this.#busy = true;
    this.#render();
    try {
      const response = await fetch(this.#url(`/proposals/${encodeURIComponent(proposal.id)}/${action}`), {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ command_id: pending.command_id }),
      });
      const result = await response.json().catch(() => ({}));
      if (!response.ok && !(response.status === 409 && pending.sent)) {
        this.#pending = null;
        throw new Error(result.error || `${action === 'approve' ? 'Approve' : 'Reject'} failed (${response.status})`);
      }
      this.#pending = null;
      this.#busy = false;
      await this.load();
      this.#status.textContent = action === 'approve'
        ? `${destination} is allowed for this Session. The Agent's call continues.`
        : `${destination} was rejected. The Agent is told not to ask again.`;
    } catch (error) {
      if (this.#pending) this.#pending.sent = true;
      this.#status.textContent = this.#pending
        ? `The answer was lost; select ${action === 'approve' ? 'Approve' : 'Reject'} again to resend the same request. (${error.message})`
        : error.message;
    } finally {
      this.#busy = false;
      this.#render();
    }
  }

  async #allow(row) {
    if (this.#busy) return;
    const pending = this.#pending?.host === row.host && this.#pending?.port === row.port && this.#pending?.scope === row.scope
      ? this.#pending
      : { host: row.host, port: row.port, scope: row.scope, command_id: crypto.randomUUID() };
    this.#pending = pending;
    this.#busy = true;
    this.#render();
    try {
      const response = await fetch(this.#url('/allow'), {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ command_id: pending.command_id, scope: pending.scope, host: pending.host, ports: [pending.port] }),
      });
      const result = await response.json().catch(() => ({}));
      if (!response.ok && !(response.status === 409 && pending.sent)) {
        this.#pending = null;
        throw new Error(result.error || `Allow failed (${response.status})`);
      }
      this.#pending = null;
      this.#busy = false;
      await this.load();
      this.#status.textContent = `${pending.host}:${pending.port} is allowed for this Session. New connections use it now.`;
    } catch (error) {
      if (this.#pending) this.#pending.sent = true;
      this.#status.textContent = this.#pending
        ? `The answer was lost; select Allow again to resend the same request. (${error.message})`
        : error.message;
    } finally {
      this.#busy = false;
      this.#render();
    }
  }
}

customElements.define('ax-session-network', AxSessionNetwork);
