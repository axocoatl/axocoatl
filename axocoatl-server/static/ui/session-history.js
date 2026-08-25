import { adopt } from './sheets.js';

/**
 * `<ax-session-history>` keeps durable conversation history inside the one-app
 * session workflow. It owns the history/search/export dialog, but deliberately
 * does not own shell navigation or the active chat transcript.
 *
 * Integration contract:
 *
 * - Set `sessionId` and (optionally) `sessionName`, or call
 *   `show({ sessionId, sessionName, turnId, scope, query })`.
 * - Call `show()` from a Session menu or command-palette action. Call
 *   `reload()` after an externally completed turn when the dialog is open.
 * - Listen for `session-turn-open`; detail is `{ sessionId, turnId, turn }`.
 *   The shell should open that session, then reveal/scroll to `turnId`.
 * - Listen for `session-history-rewound`; detail is
 *   `{ sessionId, keepThroughTurnId, response }`. The shell should invalidate
 *   its visible transcript for that session.
 * - `close` is emitted after the dialog closes. `notify` mirrors export and
 *   rewind outcomes as `{ title, body, kind }` for the shell's toast system.
 *
 * The component calls these canonical endpoints:
 *
 * - `GET /api/sessions/:id/turns`
 * - `GET /api/session-turns/search?q=&session_id=` (`session_id` is optional)
 * - `GET /api/sessions/:id/export?format=markdown|json`
 * - `POST /api/sessions/:id/rewind` with `{ keep_through_turn_id }`
 *
 * @element ax-session-history
 * @attr {boolean} open
 * @attr {string} session-id
 * @attr {string} session-name
 * @fires session-turn-open
 * @fires session-history-rewound
 * @fires close
 * @fires notify
 */

const FOCUSABLE = [
  'button:not([disabled])',
  '[href]',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',');

const CSS = `
:host { display: none; }
:host([open]) {
  position: fixed; inset: 0; z-index: 4400; display: block;
  color: var(--text); font: var(--fs-body) / var(--lh-body) var(--font-sans);
}
* { box-sizing: border-box; }
button, input, select { font: inherit; }
button:focus-visible, input:focus-visible, select:focus-visible, [tabindex]:focus-visible {
  outline: none; box-shadow: var(--focus-ring);
}
.backdrop {
  position: absolute; inset: 0; display: grid; place-items: center; padding: var(--sp-4);
  background: rgba(0, 0, 0, .58);
}
.dialog {
  display: flex; width: min(1040px, 96vw); height: min(820px, 92vh); min-height: 360px;
  flex-direction: column; overflow: hidden; border: 1px solid var(--border-strong);
  border-radius: var(--r-xl); background: var(--panel); box-shadow: var(--shadow-lg);
  animation: rise var(--dur-base) var(--ease);
}
@keyframes rise { from { opacity: 0; transform: translateY(6px); } }
.head {
  display: flex; align-items: flex-start; gap: var(--sp-3); padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--border); background: var(--panel-2);
}
.title-wrap { min-width: 0; flex: 1; }
h2 { margin: 0; font-size: var(--fs-lg); font-weight: var(--fw-bold); }
.subtitle {
  margin-top: 2px; overflow: hidden; color: var(--muted); font-size: var(--fs-sm);
  text-overflow: ellipsis; white-space: nowrap;
}
.icon-button {
  display: inline-grid; width: 30px; height: 30px; flex: 0 0 auto; place-items: center;
  border: 1px solid transparent; border-radius: var(--r-md); background: transparent;
  color: var(--muted); cursor: pointer;
}
.icon-button:hover { border-color: var(--border); background: var(--bg-3); color: var(--text); }
.tools {
  display: flex; align-items: center; gap: var(--sp-2); padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--border); background: var(--panel);
}
.search-form { display: flex; min-width: 0; flex: 1; gap: var(--sp-2); }
.scope, .search {
  min-width: 0; border: 1px solid var(--border); border-radius: var(--r-md);
  background: var(--bg-2); color: var(--text);
}
.scope { width: 138px; padding: 7px var(--sp-2); }
.search { flex: 1; padding: 7px 10px; }
.button {
  min-height: 32px; padding: 6px 11px; border: 1px solid var(--border);
  border-radius: var(--r-md); background: transparent; color: var(--text); cursor: pointer;
}
.button:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
.button.primary { border-color: var(--accent); background: var(--accent); color: #fff; }
.button.primary:hover:not(:disabled) { color: #fff; filter: brightness(1.08); }
.button.danger { border-color: var(--err); background: var(--err); color: #fff; }
.button:disabled { opacity: .48; cursor: default; filter: none; }
.exports { display: flex; gap: var(--sp-1); }
.export-label { align-self: center; color: var(--muted); font-size: var(--fs-xs); }
.announcement {
  min-height: 24px; padding: 3px var(--sp-4); border-bottom: 1px solid var(--border);
  color: var(--muted); font-size: var(--fs-xs);
}
.announcement.error { color: var(--err); }
.body { min-height: 0; flex: 1; overflow-y: auto; background: var(--bg); }
.state {
  display: grid; min-height: 250px; place-content: center; gap: var(--sp-2);
  padding: var(--sp-6) var(--sp-4); color: var(--muted); text-align: center;
}
.state strong { color: var(--text); font-size: var(--fs-lg); font-weight: var(--fw-medium); }
.state.error strong { color: var(--err); }
.state .button { justify-self: center; margin-top: var(--sp-1); }
.results { display: flex; flex-direction: column; gap: var(--sp-3); padding: var(--sp-4); }
.turn {
  border: 1px solid var(--border); border-radius: var(--r-lg); background: var(--panel);
  box-shadow: var(--shadow-sm);
}
.turn:target, .turn.highlight { border-color: var(--accent); box-shadow: var(--focus-ring); }
.turn-head {
  display: flex; align-items: center; gap: var(--sp-2); padding: var(--sp-2) var(--sp-3);
  border-bottom: 1px solid var(--border); color: var(--muted); font-size: var(--fs-xs);
}
.session-label {
  min-width: 0; max-width: 260px; overflow: hidden; color: var(--text);
  font-weight: var(--fw-medium); text-overflow: ellipsis; white-space: nowrap;
}
.status {
  display: inline-flex; align-items: center; gap: 5px; padding: 2px 7px;
  border-radius: var(--r-pill); background: var(--bg-3); color: var(--muted);
  font-size: var(--fs-xs); white-space: nowrap;
}
.status::before { width: 6px; height: 6px; border-radius: 50%; background: currentColor; content: ''; }
.status.completed { color: var(--ok); }
.status.running { color: var(--accent-2); }
.status.failed { color: var(--err); }
.status.cancelled, .status.interrupted { color: var(--warn); }
.when { margin-left: auto; white-space: nowrap; }
.turn-main {
  display: block; width: 100%; padding: var(--sp-4); border: 0; background: transparent;
  color: var(--text); cursor: pointer; text-align: left;
}
.turn-main:hover { background: color-mix(in srgb, var(--accent) 7%, transparent); }
.speaker {
  margin-bottom: var(--sp-1); color: var(--muted); font-size: var(--fs-xs);
  font-weight: var(--fw-bold); letter-spacing: .06em; text-transform: uppercase;
}
.prompt, .output {
  display: -webkit-box; overflow: hidden; overflow-wrap: anywhere; white-space: pre-wrap;
  -webkit-box-orient: vertical;
}
.prompt { -webkit-line-clamp: 3; }
.output { margin-top: var(--sp-2); color: var(--muted); line-height: 1.55; -webkit-line-clamp: 4; }
.error-text { margin-top: var(--sp-2); color: var(--err); }
.context { display: flex; flex-wrap: wrap; gap: var(--sp-1); margin-top: var(--sp-2); }
.turn > .context { margin-top: 0; padding: 0 var(--sp-3) var(--sp-3); }
.chip {
  display: inline-block;
  max-width: 240px; overflow: hidden; padding: 2px 7px; border: 1px solid var(--border);
  border-radius: var(--r-pill); background: var(--bg-3); color: var(--muted);
  font-size: var(--fs-xs); text-overflow: ellipsis; white-space: nowrap;
}
.chip[href] { color: var(--accent); text-decoration: none; }
.chip[href]:hover { text-decoration: underline; }
.matches { margin-top: var(--sp-2); color: var(--muted); font-size: var(--fs-xs); }
.route {
  margin: 0 var(--sp-4) var(--sp-3); overflow: hidden;
  border: 1px solid var(--border); border-radius: var(--r-md); background: var(--bg-2);
}
.route summary {
  padding: var(--sp-2) var(--sp-3); color: var(--text); cursor: pointer;
  font-size: var(--fs-xs); font-weight: var(--fw-medium);
}
.route summary:hover { background: var(--bg-3); }
.tool-list { display: grid; gap: var(--sp-2); padding: 0 var(--sp-3) var(--sp-3); }
.tool-evidence { display: grid; gap: var(--sp-1); padding: var(--sp-2); border-left: 2px solid var(--border-strong); }
.tool-evidence.failed { border-left-color: var(--err); }
.tool-head { display: flex; align-items: baseline; flex-wrap: wrap; gap: var(--sp-2); }
.tool-name { color: var(--text); font: var(--fw-medium) var(--fs-xs) var(--font-mono); }
.tool-agent, .tool-status { color: var(--muted); font-size: var(--fs-xs); }
.tool-status { margin-left: auto; }
.tool-evidence.failed .tool-status { color: var(--err); }
.tool-detail { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: var(--sp-2); align-items: start; }
.tool-detail span { color: var(--muted); font-size: var(--fs-xs); }
.tool-detail code {
  max-height: 9rem; overflow: auto; color: var(--text); font: var(--fs-xs) / 1.45 var(--font-mono);
  overflow-wrap: anywhere; white-space: pre-wrap;
}
.turn-foot {
  display: flex; align-items: center; gap: var(--sp-2); padding: var(--sp-2) var(--sp-4);
  border-top: 1px solid var(--border); background: var(--panel-2);
}
.turn-meta { min-width: 0; flex: 1; overflow: hidden; color: var(--muted); font-size: var(--fs-xs); text-overflow: ellipsis; white-space: nowrap; }
.text-button {
  padding: 4px 7px; border: 1px solid transparent; border-radius: var(--r-sm); background: transparent;
  color: var(--text); cursor: pointer; font-size: var(--fs-xs);
}
.text-button:hover:not(:disabled) { border-color: var(--border); background: var(--bg-3); color: var(--text); }
.text-button.rewind:hover:not(:disabled) { color: var(--warn); }
.text-button:disabled { opacity: .45; cursor: default; }
.confirm-layer {
  position: absolute; inset: 0; z-index: 2; display: grid; place-items: center;
  padding: var(--sp-4); background: rgba(0, 0, 0, .64);
}
.confirm {
  width: min(470px, 94vw); overflow: hidden; border: 1px solid var(--border-strong);
  border-radius: var(--r-xl); background: var(--panel); box-shadow: var(--shadow-lg);
}
.confirm h3 { margin: 0; padding: var(--sp-3) var(--sp-4); border-bottom: 1px solid var(--border); font-size: var(--fs-lg); }
.confirm-body { padding: var(--sp-4); color: var(--muted); }
.confirm-body p { margin: 0; white-space: pre-wrap; }
.confirm-note { margin-top: var(--sp-2) !important; color: var(--warn); font-size: var(--fs-sm); }
.confirm-error { margin-top: var(--sp-2); color: var(--err); font-size: var(--fs-sm); }
.confirm-foot { display: flex; justify-content: flex-end; gap: var(--sp-2); padding: var(--sp-3) var(--sp-4); border-top: 1px solid var(--border); }
@media (max-width: 720px) {
  .backdrop { padding: 0; }
  .dialog { width: 100vw; height: 100vh; min-height: 0; border: 0; border-radius: 0; }
  .head { padding: var(--sp-3); }
  .tools { align-items: stretch; flex-direction: column; padding: var(--sp-2) var(--sp-3); }
  .search-form { flex-wrap: wrap; }
  .scope { width: 100%; }
  .search { flex-basis: calc(100% - 84px); }
  .exports { justify-content: flex-end; }
  .announcement { padding-inline: var(--sp-3); }
  .results { padding: var(--sp-2); }
  .session-label { max-width: 150px; }
  .turn-head { align-items: flex-start; flex-wrap: wrap; }
  .when { margin-left: 0; width: 100%; }
}
`;

function deepActiveElement() {
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  return active;
}

function firstString(...values) {
  const value = values.find((candidate) => typeof candidate === 'string' && candidate.length);
  return value || '';
}

function numericTime(value) {
  if (typeof value === 'string' && /^\d+$/.test(value)) value = Number(value);
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) return 0;
  return value < 1e12 ? value * 1000 : value;
}

function turnOutput(raw) {
  const direct = firstString(raw?.final_output, raw?.finalOutput, raw?.partial_output, raw?.partialOutput, raw?.output);
  if (direct) return direct;
  const outputs = Array.isArray(raw?.agent_outputs) ? raw.agent_outputs : raw?.agentOutputs;
  if (!Array.isArray(outputs)) return '';
  return outputs
    .map((entry) => {
      const output = firstString(entry?.output, entry?.content);
      const agent = firstString(entry?.agent_id, entry?.agentId, entry?.name);
      return output ? `${agent ? `${agent}: ` : ''}${output}` : '';
    })
    .filter(Boolean)
    .join('\n\n');
}

function toolEventCount(raw) {
  return toolEvidence(raw).length;
}

function evidencePreview(value) {
  if (value == null) return '';
  if (typeof value === 'string') return value;
  if (value?.truncated === true && typeof value?.preview === 'string') {
    return `${value.preview}\n…truncated durable preview`;
  }
  try { return JSON.stringify(value, null, 2); } catch { return String(value); }
}

function toolEvidence(raw) {
  const events = Array.isArray(raw?.execution_events)
    ? raw.execution_events
    : Array.isArray(raw?.executionEvents) ? raw.executionEvents : [];
  const calls = [];
  const byIdentity = new Map();
  events.forEach((entry, index) => {
    if (!['tool_started', 'tool_result'].includes(entry?.kind)) return;
    const metadata = entry.metadata && typeof entry.metadata === 'object' ? entry.metadata : {};
    const agent = firstString(metadata.agent_id, metadata.agentId);
    const occurrence = metadata.occurrence;
    const callId = firstString(metadata.call_id_sha256, metadata.callIdSha256, metadata.call_id,
      metadata.callId, entry.execution_id, entry.executionId);
    const identity = `${agent}\u0000${occurrence == null ? `call:${callId || index}` : `occurrence:${occurrence}`}`;
    let call = byIdentity.get(identity);
    if (!call) {
      call = {
        agent,
        name: firstString(metadata.tool_name, metadata.toolName, 'tool'),
        arguments: '',
        result: '',
        failed: false,
        complete: false,
      };
      byIdentity.set(identity, call);
      calls.push(call);
    }
    if (entry.kind === 'tool_started') call.arguments = evidencePreview(metadata.arguments);
    else {
      call.result = evidencePreview(metadata.result);
      call.failed = metadata.is_error === true || metadata.isError === true;
      call.complete = true;
    }
  });
  return calls;
}

function normalizeTurn(raw, fallbackSessionId = '', matchedFields = []) {
  const turn = raw?.turn && typeof raw.turn === 'object' ? raw.turn : raw || {};
  const metadata = turn.metadata && typeof turn.metadata === 'object' ? turn.metadata : {};
  const hasTokenUsage = ['input_tokens', 'output_tokens', 'reasoning_tokens', 'token_usage_known']
    .some((key) => Object.prototype.hasOwnProperty.call(metadata, key));
  const inputTokens = Number(metadata.input_tokens || 0);
  const outputTokens = Number(metadata.output_tokens || 0);
  const reasoningTokens = Number(metadata.reasoning_tokens || 0);
  const tokenUsageKnown = metadata.token_usage_known === true;
  const context = Array.isArray(turn.context)
    ? turn.context
    : Array.isArray(turn.context_refs)
      ? turn.context_refs
      : [];
  const sessionId = firstString(turn.session_id, turn.sessionId, fallbackSessionId);
  return {
    id: firstString(turn.id, turn.turn_id, turn.turnId),
    sessionId,
    sessionName: firstString(turn.session_name, turn.sessionName, metadata.session_name, metadata.sessionName),
    userInput: firstString(turn.user_input, turn.userInput, turn.input, turn.prompt),
    output: turnOutput(turn),
    error: firstString(turn.error, turn.error_message, turn.errorMessage),
    status: firstString(turn.status, turn.lifecycle, 'completed').toLowerCase(),
    agentId: firstString(turn.agent_id, turn.agentId),
    model: firstString(turn.model, turn.model_id, turn.modelId),
    toolCount: toolEventCount(turn),
    tools: toolEvidence(turn),
    context,
    createdAt: numericTime(turn.created_at ?? turn.createdAt ?? turn.started_at ?? turn.startedAt),
    updatedAt: numericTime(turn.updated_at ?? turn.updatedAt ?? turn.completed_at ?? turn.completedAt),
    superseded: Boolean(turn.superseded),
    matchedFields: Array.isArray(matchedFields) ? matchedFields.map(String) : [],
    usageLabel: hasTokenUsage
      ? `${tokenUsageKnown ? '' : '≥'}${inputTokens + outputTokens + reasoningTokens} tokens`
        + `${tokenUsageKnown ? '' : ' known subtotal'}`
      : '',
    raw: turn,
  };
}

function collection(body, keys) {
  if (Array.isArray(body)) return body;
  for (const key of keys) {
    if (Array.isArray(body?.[key])) return body[key];
  }
  return [];
}

function displayTime(value) {
  if (!value) return 'Time unavailable';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return 'Time unavailable';
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: 'medium', timeStyle: 'short',
  }).format(date);
}

function displayText(value, limit) {
  const text = String(value || '');
  return text.length > limit ? `${text.slice(0, Math.max(0, limit - 1))}…` : text;
}

function statusLabel(status) {
  return ({
    running: 'Running', completed: 'Completed', failed: 'Failed',
    cancelled: 'Cancelled', interrupted: 'Interrupted',
  })[status] || status || 'Unknown';
}

function matchLabel(field) {
  return ({
    user_input: 'request', userInput: 'request', output: 'response', error: 'error', context: 'context',
  })[field] || String(field).replaceAll('_', ' ');
}

function safeFileStem(value) {
  const stem = String(value || 'axocoatl-session')
    .normalize('NFKD')
    .replace(/[^a-zA-Z0-9._-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 80);
  return stem || 'axocoatl-session';
}

function responseFilename(response, fallback) {
  const disposition = response.headers.get('content-disposition') || '';
  const utf8 = disposition.match(/filename\*=UTF-8''([^;]+)/i);
  if (utf8) {
    try {
      return decodeURIComponent(utf8[1].replace(/^"|"$/g, '')).trim().replace(/[\\/\0]/g, '-');
    } catch { /* use fallback */ }
  }
  const plain = disposition.match(/filename="([^"]+)"|filename=([^;]+)/i);
  return (plain?.[1] || plain?.[2] || fallback).trim().replace(/[\\/\0]/g, '-');
}

export class AxSessionHistory extends HTMLElement {
  static get observedAttributes() { return ['open', 'session-id', 'session-name', 'rewind-enabled']; }

  #root;
  #phase = 'idle';
  #error = '';
  #turns = [];
  #searchResults = [];
  #query = '';
  #scope = 'session';
  #returnFocus = null;
  #requestController = null;
  #requestGeneration = 0;
  #refreshQueued = false;
  #searchTimer = null;
  #highlightTurnId = '';
  #confirm = null;
  #exporting = '';
  #announcement = '';
  #announcementKind = '';
  #boundDocumentKeydown;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="backdrop" data-action="backdrop">
        <section class="dialog" role="dialog" aria-modal="true" aria-labelledby="session-history-title" tabindex="-1">
          <header class="head">
            <div class="title-wrap">
              <h2 id="session-history-title">Session history</h2>
              <div class="subtitle"></div>
            </div>
            <button class="icon-button close" type="button" aria-label="Close session history" title="Close">&#x2715;</button>
          </header>
          <div class="tools">
            <form class="search-form" role="search">
              <select class="scope" aria-label="Search scope">
                <option value="session">This session</option>
                <option value="all">All sessions</option>
              </select>
              <input class="search" type="search" aria-label="Search session turns" placeholder="Search requests, responses, and context" autocomplete="off">
              <button class="button search-submit" type="submit">Search history</button>
            </form>
            <div class="exports" aria-label="Export this Session">
              <span class="export-label">Export this Session</span>
              <button class="button export-markdown" type="button" data-format="markdown"
                aria-label="Export this Session as Markdown">Markdown</button>
              <button class="button export-json" type="button" data-format="json"
                aria-label="Export this Session as JSON">JSON</button>
            </div>
          </div>
          <div class="announcement" aria-live="polite"></div>
          <main class="body"></main>
        </section>
        <div class="confirm-host"></div>
      </div>`;
    adopt(this.#root, CSS, []);

    this.#boundDocumentKeydown = (event) => this.#onDocumentKeydown(event);
    this.#root.querySelector('.close').addEventListener('click', () => this.hide());
    this.#root.querySelector('.backdrop').addEventListener('click', (event) => {
      if (event.target === event.currentTarget) this.hide();
    });
    this.#root.querySelector('.search-form').addEventListener('submit', (event) => {
      event.preventDefault();
      this.#query = this.#root.querySelector('.search').value.trim();
      this.#cancelSearchTimer();
      void this.#load();
    });
    this.#root.querySelector('.search').addEventListener('input', (event) => {
      this.#query = event.target.value.trim();
      this.#cancelSearchTimer();
      this.#searchTimer = window.setTimeout(() => void this.#load(), 260);
    });
    this.#root.querySelector('.scope').addEventListener('change', (event) => {
      this.#scope = event.target.value === 'all' ? 'all' : 'session';
      void this.#load();
    });
    this.#root.querySelector('.exports').addEventListener('click', (event) => {
      const button = event.target.closest('[data-format]');
      if (button) void this.#export(button.dataset.format);
    });
    this.#root.querySelector('.body').addEventListener('click', (event) => this.#onBodyClick(event));
    this.#root.querySelector('.confirm-host').addEventListener('click', (event) => this.#onConfirmClick(event));
    this.#render();
  }

  connectedCallback() {
    document.addEventListener('keydown', this.#boundDocumentKeydown);
  }

  disconnectedCallback() {
    document.removeEventListener('keydown', this.#boundDocumentKeydown);
    this.#requestController?.abort();
    this.#cancelSearchTimer();
  }

  attributeChangedCallback(name, oldValue, newValue) {
    if (oldValue === newValue) return;
    if (name === 'open') {
      if (this.open) this.#didOpen();
      else this.#didClose();
      return;
    }
    if (name === 'session-id' && !this.sessionId && this.#scope === 'session') this.#scope = 'all';
    if (name === 'session-name') this.#render();
    else if (this.open) this.#queueRefresh();
    else this.#render();
  }

  get open() { return this.hasAttribute('open'); }
  set open(value) { value ? this.setAttribute('open', '') : this.removeAttribute('open'); }

  get sessionId() { return this.getAttribute('session-id') || ''; }
  set sessionId(value) {
    if (value) this.setAttribute('session-id', String(value));
    else this.removeAttribute('session-id');
  }

  get sessionName() { return this.getAttribute('session-name') || ''; }
  set sessionName(value) {
    if (value) this.setAttribute('session-name', String(value));
    else this.removeAttribute('session-name');
  }

  get rewindEnabled() { return this.getAttribute('rewind-enabled') !== 'false'; }
  set rewindEnabled(value) { this.setAttribute('rewind-enabled', value ? 'true' : 'false'); }

  /**
   * Open history around a session. Every option is optional so a preconfigured
   * element can simply call `show()`.
   */
  show({ sessionId, sessionName, turnId = '', scope, query = '', rewindEnabled } = {}) {
    const wasOpen = this.open;
    if (sessionId !== undefined) this.sessionId = sessionId;
    if (sessionName !== undefined) this.sessionName = sessionName;
    if (rewindEnabled !== undefined) this.rewindEnabled = Boolean(rewindEnabled);
    if (scope) this.#scope = scope === 'all' ? 'all' : 'session';
    else this.#scope = this.sessionId ? 'session' : 'all';
    this.#query = String(query || '').trim();
    this.#highlightTurnId = String(turnId || '');
    if (!this.open) this.#returnFocus = deepActiveElement();
    this.open = true;
    if (wasOpen) {
      this.#render();
      void this.#load();
      queueMicrotask(() => this.open && this.#root.querySelector('.search')?.focus());
    }
  }

  hide() {
    if (!this.open) return;
    this.open = false;
    this.dispatchEvent(new CustomEvent('close', { bubbles: true, composed: true }));
  }

  /** Refresh canonical data without changing the current query or scope. */
  reload() {
    if (this.open) return this.#load();
    return Promise.resolve();
  }

  #didOpen() {
    if (!this.#returnFocus) this.#returnFocus = deepActiveElement();
    if (!this.sessionId && this.#scope === 'session') this.#scope = 'all';
    this.#root.querySelector('.search').value = this.#query;
    this.#root.querySelector('.scope').value = this.#scope;
    this.#render();
    void this.#load();
    queueMicrotask(() => this.open && this.#root.querySelector('.search')?.focus());
  }

  #didClose() {
    this.#requestController?.abort();
    this.#requestController = null;
    this.#cancelSearchTimer();
    this.#confirm = null;
    this.#renderConfirm();
    const target = this.#returnFocus;
    this.#returnFocus = null;
    queueMicrotask(() => target?.isConnected && target.focus?.());
  }

  #queueRefresh() {
    if (this.#refreshQueued) return;
    this.#refreshQueued = true;
    queueMicrotask(() => {
      this.#refreshQueued = false;
      if (this.open) void this.#load();
    });
  }

  #cancelSearchTimer() {
    if (this.#searchTimer !== null) window.clearTimeout(this.#searchTimer);
    this.#searchTimer = null;
  }

  async #load() {
    if (!this.open) return;
    const sessionId = this.sessionId;
    const query = this.#query;
    const scope = this.#scope;
    if (scope === 'session' && !sessionId) {
      this.#scope = 'all';
      this.#root.querySelector('.scope').value = 'all';
    }
    if (!query && this.#scope === 'all') {
      this.#requestController?.abort();
      this.#phase = 'idle';
      this.#error = '';
      this.#searchResults = [];
      this.#render();
      return;
    }

    this.#requestController?.abort();
    const controller = new AbortController();
    const generation = ++this.#requestGeneration;
    this.#requestController = controller;
    this.#phase = 'loading';
    this.#error = '';
    this.#render();

    try {
      let body;
      if (query) {
        const parameters = new URLSearchParams({ q: query });
        if (this.#scope === 'session' && sessionId) parameters.set('session_id', sessionId);
        body = await this.#request(`/api/session-turns/search?${parameters}`, { signal: controller.signal });
        if (generation !== this.#requestGeneration || controller.signal.aborted) return;
        const hits = collection(body, ['results', 'hits', 'turns']);
        this.#searchResults = hits.map((hit) => normalizeTurn(
          hit,
          this.#scope === 'session' ? sessionId : '',
          hit?.matched_fields ?? hit?.matchedFields ?? [],
        ));
      } else {
        body = await this.#request(`/api/sessions/${encodeURIComponent(sessionId)}/turns`, { signal: controller.signal });
        if (generation !== this.#requestGeneration || controller.signal.aborted) return;
        this.#turns = collection(body, ['turns', 'results'])
          .map((turn) => normalizeTurn(turn, sessionId))
          .filter((turn) => !turn.superseded);
      }
      if (generation !== this.#requestGeneration || controller.signal.aborted) return;
      this.#phase = 'ready';
      this.#error = '';
      this.#render();
      this.#focusHighlightedTurn();
    } catch (error) {
      if (controller.signal.aborted || generation !== this.#requestGeneration) return;
      this.#phase = 'error';
      this.#error = error instanceof Error ? error.message : String(error);
      this.#render();
    } finally {
      if (this.#requestController === controller) this.#requestController = null;
    }
  }

  async #request(url, options = {}) {
    const response = await fetch(url, options);
    const contentType = response.headers.get('content-type') || '';
    const body = contentType.includes('json') ? await response.json().catch(() => ({})) : await response.text();
    if (!response.ok) {
      const message = typeof body === 'object' ? body?.error : body;
      throw new Error(message || `Request failed (HTTP ${response.status})`);
    }
    if (body && typeof body === 'object' && body.error) throw new Error(body.error);
    return body;
  }

  #render() {
    const subtitle = this.#root.querySelector('.subtitle');
    const scope = this.#root.querySelector('.scope');
    const search = this.#root.querySelector('.search');
    const sessionLabel = this.sessionName || this.sessionId;
    subtitle.textContent = sessionLabel ? sessionLabel : 'Search durable work across sessions';
    scope.value = this.#scope;
    scope.querySelector('[value="session"]').disabled = !this.sessionId;
    search.value = this.#query;
    for (const button of this.#root.querySelectorAll('[data-format]')) {
      button.disabled = !this.sessionId || Boolean(this.#exporting);
      const format = button.dataset.format;
      button.textContent = this.#exporting === format
        ? 'Exporting…'
        : format === 'json' ? 'JSON' : 'Markdown';
    }
    this.#renderAnnouncement();
    this.#renderBody();
    this.#renderConfirm();
  }

  #renderAnnouncement() {
    const node = this.#root.querySelector('.announcement');
    node.className = `announcement${this.#announcementKind === 'error' ? ' error' : ''}`;
    if (this.#phase === 'loading') node.textContent = this.#query ? 'Searching session turns…' : 'Loading session history…';
    else if (this.#announcement) node.textContent = this.#announcement;
    else if (this.#phase === 'ready') {
      const count = this.#query ? this.#searchResults.length : this.#turns.length;
      node.textContent = `${count} ${count === 1 ? 'turn' : 'turns'}`;
    } else node.textContent = '';
  }

  #renderBody() {
    const body = this.#root.querySelector('.body');
    body.replaceChildren();
    if (this.#phase === 'loading') {
      body.append(this.#state('Loading…', this.#query ? 'Searching requests, responses, and context.' : 'Reading durable session turns.'));
      return;
    }
    if (this.#phase === 'error') {
      const state = this.#state('History could not be loaded', this.#error, 'error');
      const retry = document.createElement('button');
      retry.type = 'button'; retry.className = 'button'; retry.textContent = 'Try again';
      retry.addEventListener('click', () => void this.#load()); state.append(retry); body.append(state);
      return;
    }
    if (!this.#query && this.#scope === 'all') {
      body.append(this.#state('Search all sessions', 'Enter a phrase to find matching requests, responses, errors, or attached context.'));
      return;
    }
    const turns = this.#query ? this.#searchResults : this.#turns;
    if (!turns.length) {
      body.append(this.#state(
        this.#query ? 'No matching turns' : 'No session history yet',
        this.#query ? 'Try different words or broaden the search to all sessions.' : 'Completed, failed, cancelled, and interrupted turns will appear here.',
      ));
      return;
    }
    const list = document.createElement('div'); list.className = 'results';
    const displayTurns = this.#query ? turns : turns.slice().reverse();
    displayTurns.forEach((turn) => list.append(this.#turnCard(turn)));
    body.append(list);
  }

  #state(title, detail, kind = '') {
    const state = document.createElement('section'); state.className = `state${kind ? ` ${kind}` : ''}`;
    state.setAttribute('role', kind === 'error' ? 'alert' : 'status');
    const strong = document.createElement('strong'); strong.textContent = title;
    const text = document.createElement('span'); text.textContent = detail;
    state.append(strong, text); return state;
  }

  #turnCard(turn) {
    const card = document.createElement('article');
    card.className = `turn${turn.id === this.#highlightTurnId ? ' highlight' : ''}`;
    if (turn.id) card.id = `session-turn-${turn.id}`;
    card.dataset.turnId = turn.id;
    card.dataset.sessionId = turn.sessionId;

    const head = document.createElement('div'); head.className = 'turn-head';
    if (this.#scope === 'all' || (turn.sessionId && turn.sessionId !== this.sessionId)) {
      const session = document.createElement('span'); session.className = 'session-label';
      session.textContent = turn.sessionName || turn.sessionId || 'Unknown session'; head.append(session);
    }
    const status = document.createElement('span'); status.className = `status ${turn.status}`;
    status.textContent = statusLabel(turn.status); head.append(status);
    if (turn.agentId) {
      const agent = document.createElement('span'); agent.textContent = turn.agentId; head.append(agent);
    }
    const when = document.createElement('time'); when.className = 'when';
    const createdDate = new Date(turn.createdAt);
    if (turn.createdAt && !Number.isNaN(createdDate.getTime())) when.dateTime = createdDate.toISOString();
    when.textContent = displayTime(turn.createdAt); head.append(when); card.append(head);

    const open = document.createElement('button'); open.type = 'button'; open.className = 'turn-main';
    open.dataset.action = 'open-turn';
    open.setAttribute('aria-label', `Open this turn in ${turn.sessionName || turn.sessionId || 'its Session'}`);
    const userLabel = document.createElement('div'); userLabel.className = 'speaker'; userLabel.textContent = 'Request';
    const prompt = document.createElement('div'); prompt.className = 'prompt'; prompt.textContent = displayText(turn.userInput, 4000) || '(No request text recorded)';
    open.append(userLabel, prompt);
    if (turn.output) {
      const output = document.createElement('div'); output.className = 'output'; output.textContent = displayText(turn.output, 6000); open.append(output);
    }
    if (turn.error) {
      const error = document.createElement('div'); error.className = 'error-text'; error.textContent = turn.error; open.append(error);
    }
    let contextBlock = null;
    if (turn.context.length) {
      const context = document.createElement('div'); context.className = 'context'; context.setAttribute('aria-label', 'Context used');
      turn.context.slice(0, 4).forEach((reference) => {
        const referenceId = firstString(reference?.reference_id, reference?.referenceId, reference?.id);
        const uploaded = firstString(reference?.kind).toLowerCase() === 'upload'
          && turn.sessionId && referenceId;
        const chip = document.createElement(uploaded ? 'a' : 'span'); chip.className = 'chip';
        chip.textContent = firstString(reference?.display_name, reference?.displayName, reference?.name, referenceId, 'Context');
        if (uploaded) {
          chip.href = `/api/sessions/${encodeURIComponent(turn.sessionId)}/attachments/${encodeURIComponent(referenceId)}/content`;
          chip.target = '_blank';
          chip.rel = 'noopener noreferrer';
          chip.title = 'Preview or download the immutable attachment used by this turn';
          chip.addEventListener('click', (event) => event.stopPropagation());
        }
        context.append(chip);
      });
      if (turn.context.length > 4) {
        const more = document.createElement('span'); more.className = 'chip'; more.textContent = `+${turn.context.length - 4} more`; context.append(more);
      }
      contextBlock = context;
    }
    if (turn.matchedFields.length) {
      const matches = document.createElement('div'); matches.className = 'matches';
      matches.textContent = `Matched ${turn.matchedFields.map(matchLabel).join(', ')}`; open.append(matches);
    }
    card.append(open);
    if (contextBlock) card.append(contextBlock);
    if (turn.tools.length) {
      const route = document.createElement('details'); route.className = 'route';
      // Most turns use only a few tools. Keep that durable evidence visible;
      // longer routes remain compact while their summary still names the work.
      route.open = turn.tools.length <= 3;
      const summary = document.createElement('summary');
      const toolNames = [...new Set(turn.tools.map((tool) => tool.name))];
      const names = toolNames.slice(0, 3).join(', ');
      summary.textContent = `Route · ${turn.tools.length} tool ${turn.tools.length === 1 ? 'call' : 'calls'}${names ? ` · ${names}${toolNames.length > 3 ? ', …' : ''}` : ''}`;
      route.append(summary);
      const list = document.createElement('div'); list.className = 'tool-list';
      turn.tools.forEach((tool) => {
        const evidence = document.createElement('section');
        evidence.className = `tool-evidence${tool.failed ? ' failed' : ''}`;
        const head = document.createElement('div'); head.className = 'tool-head';
        const name = document.createElement('span'); name.className = 'tool-name'; name.textContent = tool.name;
        head.append(name);
        if (tool.agent) {
          const agent = document.createElement('span'); agent.className = 'tool-agent';
          agent.textContent = `Agent: ${tool.agent}`; head.append(agent);
        }
        const status = document.createElement('span'); status.className = 'tool-status';
        status.textContent = tool.failed ? 'Failed' : tool.complete ? 'Completed' : 'Started';
        head.append(status); evidence.append(head);
        const appendDetail = (label, value) => {
          if (!value) return;
          const detail = document.createElement('div'); detail.className = 'tool-detail';
          const detailLabel = document.createElement('span'); detailLabel.textContent = label;
          const content = document.createElement('code'); content.textContent = displayText(value, 8000);
          detail.append(detailLabel, content); evidence.append(detail);
        };
        appendDetail('Input', tool.arguments);
        appendDetail(tool.failed ? 'Error' : 'Result', tool.result);
        list.append(evidence);
      });
      route.append(list); card.append(route);
    }

    const foot = document.createElement('div'); foot.className = 'turn-foot';
    const meta = document.createElement('div'); meta.className = 'turn-meta';
    meta.textContent = [
      turn.model,
      turn.usageLabel,
      turn.toolCount ? `${turn.toolCount} tool ${turn.toolCount === 1 ? 'call' : 'calls'}` : '',
      turn.context.length ? `${turn.context.length} context ${turn.context.length === 1 ? 'item' : 'items'}` : '',
    ].filter(Boolean).join(' · ');
    foot.append(meta);
    if (!this.#query && turn.sessionId === this.sessionId && this.rewindEnabled) {
      const index = this.#turns.findIndex((candidate) => candidate.id === turn.id);
      const laterCount = index < 0 ? 0 : this.#turns.length - index - 1;
      const hasRunningTurn = this.#turns.some((candidate) => candidate.status === 'running');
      const rewind = document.createElement('button'); rewind.type = 'button'; rewind.className = 'text-button rewind';
      rewind.dataset.action = 'rewind'; rewind.textContent = 'Rewind history here';
      rewind.disabled = laterCount === 0 || hasRunningTurn;
      rewind.title = hasRunningTurn
        ? 'Stop the running turn before rewinding'
        : laterCount === 0
          ? 'This is already the latest turn'
          : `Supersede ${laterCount} later ${laterCount === 1 ? 'turn' : 'turns'}; files and tool effects are not undone`;
      foot.append(rewind);
    }
    const openText = document.createElement('button'); openText.type = 'button'; openText.className = 'text-button';
    openText.dataset.action = 'open-turn'; openText.textContent = 'Open in Session';
    openText.setAttribute('aria-label', `Open this turn in ${turn.sessionName || turn.sessionId || 'its Session'}`);
    foot.append(openText);
    card.append(foot);
    return card;
  }

  #onBodyClick(event) {
    const action = event.target.closest('[data-action]');
    if (!action) return;
    const card = action.closest('.turn');
    const source = (this.#query ? this.#searchResults : this.#turns)
      .find((turn) => turn.id === card?.dataset.turnId && turn.sessionId === card?.dataset.sessionId);
    if (!source) return;
    if (action.dataset.action === 'open-turn') {
      this.dispatchEvent(new CustomEvent('session-turn-open', {
        detail: { sessionId: source.sessionId, turnId: source.id, turn: source.raw },
        bubbles: true, composed: true,
      }));
      this.hide();
      return;
    }
    if (action.dataset.action === 'rewind') this.#askRewind(source, action);
  }

  #askRewind(turn, returnFocus) {
    const index = this.#turns.findIndex((candidate) => candidate.id === turn.id);
    const laterCount = index < 0 ? 0 : this.#turns.length - index - 1;
    if (!turn.id || !this.sessionId || turn.sessionId !== this.sessionId || laterCount <= 0) return;
    this.#confirm = {
      sessionId: String(this.sessionId),
      sessionName: String(this.sessionName || this.sessionId),
      turnId: String(turn.id),
      prompt: String(turn.userInput || ''),
      laterCount,
      busy: false,
      error: '',
      returnFocus,
    };
    this.#renderConfirm();
    queueMicrotask(() => this.#root.querySelector('.confirm [data-action="cancel-rewind"]')?.focus());
  }

  #renderConfirm() {
    const host = this.#root.querySelector('.confirm-host'); host.replaceChildren();
    const confirmation = this.#confirm; if (!confirmation || !this.open) return;
    const layer = document.createElement('div'); layer.className = 'confirm-layer';
    const dialog = document.createElement('section'); dialog.className = 'confirm';
    dialog.setAttribute('role', 'alertdialog'); dialog.setAttribute('aria-modal', 'true');
    dialog.setAttribute('aria-labelledby', 'session-rewind-title'); dialog.setAttribute('aria-describedby', 'session-rewind-detail');
    const title = document.createElement('h3'); title.id = 'session-rewind-title'; title.textContent = 'Rewind this session?';
    const body = document.createElement('div'); body.className = 'confirm-body';
    const detail = document.createElement('p'); detail.id = 'session-rewind-detail';
    const preview = confirmation.prompt.length > 120 ? `${confirmation.prompt.slice(0, 117)}…` : confirmation.prompt;
    detail.textContent = `Keep “${preview || 'the selected turn'}” as the latest turn in ${confirmation.sessionName}.`;
    const note = document.createElement('p'); note.className = 'confirm-note';
    note.textContent = `${confirmation.laterCount} later ${confirmation.laterCount === 1 ? 'turn' : 'turns'} will be superseded in normal Session history. Files and tool effects are not undone.`;
    body.append(detail, note);
    if (confirmation.error) {
      const error = document.createElement('div'); error.className = 'confirm-error'; error.setAttribute('role', 'alert'); error.textContent = confirmation.error; body.append(error);
    }
    const foot = document.createElement('div'); foot.className = 'confirm-foot';
    const cancel = document.createElement('button'); cancel.type = 'button'; cancel.className = 'button'; cancel.dataset.action = 'cancel-rewind';
    cancel.textContent = 'Cancel'; cancel.disabled = confirmation.busy;
    const accept = document.createElement('button'); accept.type = 'button'; accept.className = 'button danger'; accept.dataset.action = 'confirm-rewind';
    accept.textContent = confirmation.busy ? 'Rewinding…' : 'Rewind session'; accept.disabled = confirmation.busy;
    foot.append(cancel, accept); dialog.append(title, body, foot); layer.append(dialog); host.append(layer);
  }

  #onConfirmClick(event) {
    const action = event.target.closest('[data-action]')?.dataset.action;
    if (action === 'cancel-rewind') this.#closeConfirm();
    else if (action === 'confirm-rewind') void this.#rewind();
  }

  #closeConfirm() {
    const confirmation = this.#confirm; if (!confirmation || confirmation.busy) return;
    this.#confirm = null; this.#renderConfirm();
    queueMicrotask(() => confirmation.returnFocus?.isConnected && confirmation.returnFocus.focus?.());
  }

  async #rewind() {
    const confirmation = this.#confirm;
    if (!confirmation || confirmation.busy) return;
    // Capture identity once. Attribute changes while the request is in flight
    // must never redirect an irreversible operation to another session.
    const sessionId = confirmation.sessionId;
    const keepThroughTurnId = confirmation.turnId;
    confirmation.busy = true; confirmation.error = ''; this.#renderConfirm();
    try {
      const response = await this.#request(`/api/sessions/${encodeURIComponent(sessionId)}/rewind`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ keep_through_turn_id: keepThroughTurnId }),
      });
      this.#confirm = null;
      this.#announce('Session history rewound.', 'ok');
      this.#emitNotify('Session rewound', confirmation.sessionName, 'ok');
      this.dispatchEvent(new CustomEvent('session-history-rewound', {
        detail: { sessionId, keepThroughTurnId, response }, bubbles: true, composed: true,
      }));
      if (this.open && this.sessionId === sessionId) await this.#load();
      else this.#renderConfirm();
    } catch (error) {
      if (this.#confirm !== confirmation) return;
      confirmation.busy = false;
      confirmation.error = error instanceof Error ? error.message : String(error);
      this.#renderConfirm();
      queueMicrotask(() => this.#root.querySelector('.confirm [data-action="confirm-rewind"]')?.focus());
    }
  }

  async #export(format) {
    if (!['markdown', 'json'].includes(format) || this.#exporting || !this.sessionId) return;
    // As with rewind, export the session the person acted on even if the shell
    // changes selection before the response arrives.
    const sessionId = String(this.sessionId);
    const sessionName = String(this.sessionName || this.sessionId);
    this.#exporting = format; this.#announce('', ''); this.#render();
    try {
      const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/export?format=${encodeURIComponent(format)}`);
      if (!response.ok) {
        const body = await response.json().catch(() => ({}));
        throw new Error(body?.error || `Export failed (HTTP ${response.status})`);
      }
      const blob = await response.blob();
      const extension = format === 'json' ? 'json' : 'md';
      const filename = responseFilename(response, `${safeFileStem(sessionName)}.${extension}`);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a'); anchor.href = url; anchor.download = filename;
      anchor.style.display = 'none'; this.#root.append(anchor); anchor.click(); anchor.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      this.#announce(`${filename} downloaded.`, 'ok');
      this.#emitNotify('Session exported', filename, 'ok');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.#announce(message, 'error');
      this.#emitNotify('Export failed', message, 'error');
    } finally {
      this.#exporting = ''; this.#render();
    }
  }

  #announce(message, kind = '') {
    this.#announcement = message; this.#announcementKind = kind; this.#renderAnnouncement();
  }

  #emitNotify(title, body, kind) {
    this.dispatchEvent(new CustomEvent('notify', {
      detail: { title, body, kind }, bubbles: true, composed: true,
    }));
  }

  #focusHighlightedTurn() {
    if (!this.#highlightTurnId) return;
    const selector = `#session-turn-${globalThis.CSS.escape(this.#highlightTurnId)}`;
    const card = this.#root.querySelector(selector);
    if (!card) return;
    card.scrollIntoView({ block: 'center' });
    card.querySelector('[data-action="open-turn"]')?.focus();
    this.#highlightTurnId = '';
  }

  #focusables() {
    const container = this.#confirm
      ? this.#root.querySelector('.confirm')
      : this.#root.querySelector('.dialog');
    if (!container) return [];
    return Array.from(container.querySelectorAll(FOCUSABLE))
      .filter((element) => !element.hidden && element.getClientRects().length > 0);
  }

  #onDocumentKeydown(event) {
    if (!this.open) return;
    if (event.key === 'Escape') {
      event.preventDefault(); event.stopPropagation();
      if (this.#confirm && !this.#confirm.busy) this.#closeConfirm();
      else if (!this.#confirm) this.hide();
      return;
    }
    if (event.key !== 'Tab') return;
    const focusable = this.#focusables();
    if (!focusable.length) { event.preventDefault(); this.#root.querySelector('.dialog')?.focus(); return; }
    const current = event.composedPath()[0];
    const index = focusable.indexOf(current);
    if (event.shiftKey && index <= 0) {
      event.preventDefault(); focusable[focusable.length - 1].focus();
    } else if (!event.shiftKey && (index < 0 || index === focusable.length - 1)) {
      event.preventDefault(); focusable[0].focus();
    }
  }
}

if (!customElements.get('ax-session-history')) customElements.define('ax-session-history', AxSessionHistory);
