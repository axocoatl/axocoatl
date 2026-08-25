import { adopt } from './sheets.js';

/**
 * `<ax-session-context>` owns the files that are attached to a Session chat.
 *
 * It is deliberately a composer accessory, not a file browser or destination.
 * The shell sets `session`, asks `selectedReferenceIds()` when sending a turn,
 * and calls `consumeAccepted(ids)` only after the server accepts that turn.
 *
 * @element ax-session-context
 * @attr {string} session Session whose context is being edited.
 * @attr {boolean} disabled Prevent new uploads and mutations.
 * @attr {number} max-upload-bytes Optional client-side upload ceiling.
 * @fires context-change detail: {session, references, selectedReferenceIds}
 * @fires notify detail: {title, body, kind}
 */

export const DEFAULT_MAX_UPLOAD_BYTES = 25 * 1024 * 1024;
export const DEFAULT_MAX_IMAGE_UPLOAD_BYTES = 10 * 1024 * 1024;
export const ATTACHMENT_SCOPES = Object.freeze(['this_turn', 'session']);
const TEXT_PREVIEW_LIMIT = 256 * 1024;
const FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

const CSS = `
:host {
  position: relative; display: block; min-width: 0; color: var(--text);
  font: var(--fs-body) / var(--lh-body) var(--font-sans);
}
* { box-sizing: border-box; }
button, input { font: inherit; }
button:focus-visible, [href]:focus-visible, [tabindex]:focus-visible {
  outline: none; box-shadow: var(--focus-ring);
}
.surface { display: flex; min-width: 0; flex-direction: column; gap: var(--sp-1); }
.context-row { display: flex; min-width: 0; align-items: center; gap: var(--sp-1); }
.attach {
  display: inline-flex; width: 28px; height: 28px; flex: 0 0 28px; align-items: center;
  justify-content: center; padding: 0; border: 1px solid var(--border); border-radius: var(--r-md);
  background: transparent; color: var(--muted); cursor: pointer;
}
.attach:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
.attach:disabled { opacity: .45; cursor: not-allowed; }
.clip-icon { font-size: var(--fs-lg); line-height: 1; transform: rotate(-8deg); }
.chips { display: flex; min-width: 0; flex: 1; flex-wrap: wrap; gap: var(--sp-1); }
.chip {
  display: inline-flex; min-width: 0; max-width: min(340px, 100%); align-items: center;
  border: 1px solid var(--border); border-radius: var(--r-pill); background: var(--bg-3);
  color: var(--text); box-shadow: var(--shadow-sm);
}
.chip.busy { opacity: .58; }
.file {
  display: inline-flex; min-width: 0; align-items: center; gap: 6px; padding: 3px 3px 3px 9px;
  border: 0; border-radius: var(--r-pill) 0 0 var(--r-pill); background: transparent;
  color: inherit; cursor: pointer;
}
.file:hover:not(:disabled) .file-name { color: var(--accent); }
.file-name { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.file-size { flex: 0 0 auto; color: var(--muted-2); font: var(--fs-xs) var(--font-mono); }
.scope {
  flex: 0 0 auto; align-self: stretch; padding: 2px 7px; border: 0; border-left: 1px solid var(--border);
  background: rgba(var(--axo-jade-rgb), .12); color: var(--accent); cursor: pointer;
  font-size: var(--fs-xs); white-space: nowrap;
}
.scope.session { background: rgba(var(--axo-blue-rgb), .12); color: var(--accent-2); }
.scope:hover:not(:disabled) { filter: brightness(1.18); }
.remove {
  width: 25px; align-self: stretch; padding: 0; border: 0; border-left: 1px solid var(--border);
  border-radius: 0 var(--r-pill) var(--r-pill) 0; background: transparent; color: var(--muted-2);
  cursor: pointer;
}
.remove:hover:not(:disabled) { background: color-mix(in srgb, var(--err) 12%, transparent); color: var(--err); }
.state {
  display: flex; min-width: 0; align-items: center; gap: var(--sp-2); color: var(--muted);
  font-size: var(--fs-xs);
}
.state[hidden] { display: none; }
.state.error { color: var(--err); }
.retry {
  padding: 2px 7px; border: 1px solid currentColor; border-radius: var(--r-sm);
  background: transparent; color: inherit; cursor: pointer; font-size: var(--fs-xs);
}
.dropzone {
  display: none; min-height: 52px; align-items: center; justify-content: center;
  border: 1px dashed var(--border-strong); border-radius: var(--r-md);
  background: rgba(var(--axo-jade-rgb), .09); color: var(--muted); text-align: center;
  font-size: var(--fs-sm); pointer-events: none;
}
:host([dragging]) .dropzone { display: flex; border-color: var(--accent); color: var(--accent); }
.uploads { display: flex; flex-direction: column; gap: var(--sp-1); }
.uploads:empty { display: none; }
.upload {
  display: grid; grid-template-columns: minmax(90px, 1fr) minmax(80px, 180px) auto;
  align-items: center; gap: var(--sp-2); padding: 4px var(--sp-2); border: 1px solid var(--border);
  border-radius: var(--r-md); background: var(--panel-2); font-size: var(--fs-xs);
}
.upload-name { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.upload-status { color: var(--muted); }
.upload-status.error { color: var(--err); }
progress { width: 100%; height: 5px; accent-color: var(--accent); }
.upload-actions { display: inline-flex; align-items: center; gap: var(--sp-1); }
.upload-action {
  padding: 2px 6px; border: 0; border-radius: var(--r-sm); background: transparent;
  color: var(--muted); cursor: pointer; font-size: var(--fs-xs);
}
.upload-action:hover { background: var(--bg-3); color: var(--text); }
.overlay {
  position: fixed; inset: 0; z-index: 1500; display: flex; align-items: center; justify-content: center;
  padding: var(--sp-4); background: rgba(0, 0, 0, .58);
}
.dialog {
  display: flex; width: min(820px, 94vw); max-height: min(820px, 92vh); min-height: 220px;
  flex-direction: column; overflow: hidden; border: 1px solid var(--border-strong);
  border-radius: var(--r-xl); background: var(--panel); box-shadow: var(--shadow-lg);
}
.dialog-head {
  display: flex; align-items: center; gap: var(--sp-2); padding: var(--sp-3) var(--sp-4);
  border-bottom: 1px solid var(--border); background: var(--panel-2);
}
.dialog-title { flex: 1; min-width: 0; margin: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: var(--fs-lg); }
.dialog-close {
  width: 30px; height: 30px; padding: 0; border: 1px solid transparent;
  border-radius: var(--r-md); background: transparent; color: var(--muted); cursor: pointer;
}
.dialog-close:hover { border-color: var(--border); color: var(--text); }
.preview {
  display: flex; min-height: 0; flex: 1; align-items: center; justify-content: center;
  overflow: auto; padding: var(--sp-4); background: var(--bg-2);
}
.preview img { display: block; max-width: 100%; max-height: 68vh; object-fit: contain; }
.preview iframe { width: 100%; min-height: 62vh; border: 1px solid var(--border); background: white; }
.preview pre {
  align-self: stretch; width: 100%; min-height: 100%; margin: 0; overflow: auto;
  color: var(--text); font: var(--fs-xs) / 1.55 var(--font-mono); white-space: pre-wrap;
  overflow-wrap: anywhere; tab-size: 2;
}
.preview-message { max-width: 520px; color: var(--muted); text-align: center; }
.preview-message.error { color: var(--err); }
.dialog-foot {
  display: flex; align-items: center; gap: var(--sp-2); padding: var(--sp-2) var(--sp-4);
  border-top: 1px solid var(--border); background: var(--panel-2);
}
.dialog-meta { flex: 1; min-width: 0; color: var(--muted); font: var(--fs-xs) var(--font-mono); }
.download {
  display: inline-flex; padding: 5px 10px; border: 1px solid var(--accent);
  border-radius: var(--r-md); background: var(--accent); color: white; text-decoration: none;
  font-size: var(--fs-sm);
}
.sr-only {
  position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
  overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
}
@media (max-width: 560px) {
  .context-row { align-items: flex-start; }
  .chips { flex-direction: column; }
  .chip { width: 100%; max-width: none; }
  .file { flex: 1; }
  .upload { grid-template-columns: minmax(0, 1fr) auto; }
  .upload progress { grid-column: 1 / -1; grid-row: 2; }
  .overlay { padding: var(--sp-2); }
  .dialog { width: 100%; max-height: 96vh; }
  .preview iframe { min-height: 68vh; }
}
`;

function stringValue(value) {
  return value == null ? '' : String(value);
}

function finiteNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : 0;
}

function booleanFlag(value) {
  if (value == null) return false;
  if (typeof value === 'string') return !['', '0', 'false', 'no', 'null'].includes(value.toLowerCase());
  return Boolean(value);
}

/** Normalize the deliberately small REST projection used by the component. */
export function normalizeSessionAttachment(raw = {}) {
  const id = stringValue(raw.reference_id ?? raw.ref_id ?? raw.attachment_id ?? raw.id);
  const scope = raw.scope === 'session' ? 'session' : 'this_turn';
  const consumedState = raw.consumed ?? raw.is_consumed ?? raw.consumed_at;
  const consumed = consumedState && typeof consumedState === 'object'
    ? consumedState.status === 'consumed'
    : booleanFlag(consumedState);
  return {
    id,
    blobId: stringValue(raw.blob_id ?? raw.blobId ?? raw.file_id),
    name: stringValue(raw.display_name ?? raw.filename ?? raw.file_name ?? raw.name) || 'Untitled attachment',
    mimeType: stringValue(raw.declared_mime ?? raw.media_type ?? raw.mime_type ?? raw.content_type ?? raw.mime) || 'application/octet-stream',
    sizeBytes: finiteNumber(raw.size_bytes ?? raw.byte_length ?? raw.size),
    scope,
    consumed,
    createdAt: raw.created_at ?? raw.createdAt ?? null,
    extraction: raw.extraction ?? raw.extraction_metadata ?? null,
    raw,
  };
}

/** Accept an array or the common API envelopes without coupling the UI to one wrapper. */
export function sessionAttachmentCollection(payload) {
  const values = Array.isArray(payload)
    ? payload
    : payload?.attachments ?? payload?.references ?? payload?.items ?? payload?.data ?? [];
  return (Array.isArray(values) ? values : [])
    .map(normalizeSessionAttachment)
    .filter((reference) => reference.id && !reference.consumed);
}

/** Pure URL contract helper, useful to route and component tests. */
export function sessionAttachmentsUrl(sessionId, referenceId = '', content = false) {
  const base = `/api/sessions/${encodeURIComponent(stringValue(sessionId))}/attachments`;
  if (!referenceId) return base;
  return `${base}/${encodeURIComponent(stringValue(referenceId))}${content ? '/content' : ''}`;
}

/** Pure selection contract: only active references are sent with the next turn. */
export function activeSessionReferenceIds(references) {
  return (Array.isArray(references) ? references : [])
    .filter((reference) => reference?.id && !reference?.consumed)
    .map((reference) => String(reference.id));
}

export function formatAttachmentBytes(bytes) {
  const size = finiteNumber(bytes);
  if (size < 1024) return `${size} B`;
  if (size < 1024 ** 2) return `${(size / 1024).toFixed(size < 10 * 1024 ? 1 : 0)} KB`;
  return `${(size / (1024 ** 2)).toFixed(size < 10 * 1024 ** 2 ? 1 : 0)} MB`;
}

export function sessionAttachmentPreviewKind(reference) {
  const mime = stringValue(reference?.mimeType).toLowerCase().split(';', 1)[0].trim();
  const extension = stringValue(reference?.name).split('.').pop()?.toLowerCase() || '';
  if (['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'image/avif'].includes(mime)) return 'image';
  if (mime === 'application/pdf' || extension === 'pdf') return 'pdf';
  if (mime.startsWith('text/') || [
    'md', 'markdown', 'txt', 'csv', 'tsv', 'json', 'jsonl', 'toml', 'yaml', 'yml',
    'js', 'mjs', 'cjs', 'ts', 'tsx', 'jsx', 'css', 'html', 'htm', 'xml', 'svg',
    'rs', 'py', 'rb', 'go', 'java', 'kt', 'swift', 'c', 'h', 'cpp', 'hpp', 'sh', 'zsh',
  ].includes(extension)) return 'text';
  return 'download';
}

function responseError(body, status) {
  return body?.error?.message || body?.error || body?.message || `Request failed (HTTP ${status})`;
}

function safeDownloadName(name) {
  return stringValue(name).replace(/[\u0000-\u001f\u007f/\\:]/g, '_').slice(0, 180) || 'attachment';
}

const HTMLElementBase = globalThis.HTMLElement ?? class {};

export class AxSessionContext extends HTMLElementBase {
  static get observedAttributes() { return ['session', 'disabled', 'max-upload-bytes']; }

  #root;
  #references = [];
  #uploads = [];
  #loadState = 'idle';
  #loadError = '';
  #loadEpoch = 0;
  #loadController = null;
  #busyReferences = new Set();
  #dragDepth = 0;
  #dialog = null;
  #dialogEpoch = 0;
  #connected = false;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="surface">
        <div class="context-row">
          <button class="attach" type="button" data-action="choose" aria-label="Attach files to this session" title="Attach files">
            <span class="clip-icon codicon codicon-attach" aria-hidden="true"></span>
          </button>
          <div class="chips" role="list" aria-label="Session context"></div>
        </div>
        <div class="state" data-role="state" role="status"></div>
        <div class="uploads" aria-label="Uploads"></div>
        <div class="dropzone" aria-hidden="true">Drop files to add them to this turn</div>
        <input class="file-input" type="file" multiple hidden />
      </div>
      <div class="dialog-host"></div>
      <div class="sr-only" data-role="announcer" aria-live="polite" aria-atomic="true"></div>`;

    this.#root.addEventListener('click', (event) => this.#onClick(event));
    this.#root.addEventListener('change', (event) => this.#onChange(event));
    this.#root.addEventListener('keydown', (event) => this.#onKeyDown(event));
    this.addEventListener('dragenter', (event) => this.#onDragEnter(event));
    this.addEventListener('dragover', (event) => this.#onDragOver(event));
    this.addEventListener('dragleave', (event) => this.#onDragLeave(event));
    this.addEventListener('drop', (event) => this.#onDrop(event));
    adopt(this.#root, CSS);
    this.#render();
  }

  connectedCallback() {
    this.#connected = true;
    if (this.session && this.#loadState === 'idle') void this.refresh();
  }

  disconnectedCallback() {
    this.#connected = false;
    this.#loadController?.abort();
    this.#dialog?.controller?.abort();
  }

  attributeChangedCallback(name, previous, next) {
    if (previous === next) return;
    if (name === 'session') {
      this.#resetForSession();
      if (this.#connected && next) void this.refresh();
    } else {
      this.#render();
    }
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(value) {
    const next = stringValue(value);
    if (next) this.setAttribute('session', next); else this.removeAttribute('session');
  }

  get disabled() { return this.hasAttribute('disabled'); }
  set disabled(value) { this.toggleAttribute('disabled', Boolean(value)); }

  get maxUploadBytes() {
    const configured = Number(this.getAttribute('max-upload-bytes'));
    return Number.isFinite(configured) && configured > 0 ? configured : DEFAULT_MAX_UPLOAD_BYTES;
  }
  set maxUploadBytes(value) {
    const next = Number(value);
    if (Number.isFinite(next) && next > 0) this.setAttribute('max-upload-bytes', String(Math.floor(next)));
    else this.removeAttribute('max-upload-bytes');
  }

  get references() { return this.#references.map((reference) => ({ ...reference })); }

  /** IDs the shell places on the next Session command. */
  selectedReferenceIds() { return activeSessionReferenceIds(this.#references); }

  /** Open immutable context captured on a historical turn. */
  openReference(value) {
    if (!this.session) return false;
    const reference = normalizeSessionAttachment(value || {});
    if (!reference.id) return false;
    this.#openDialog(reference);
    return true;
  }

  /** Reload the authoritative active references for the current Session. */
  async refresh() {
    const session = this.session;
    const epoch = ++this.#loadEpoch;
    this.#loadController?.abort();
    if (!session) {
      this.#references = [];
      this.#loadState = 'idle';
      this.#loadError = '';
      this.#render();
      this.#emitChange();
      return [];
    }

    const controller = new AbortController();
    this.#loadController = controller;
    this.#loadState = 'loading';
    this.#loadError = '';
    this.#render();
    try {
      const payload = await this.#request(sessionAttachmentsUrl(session), { signal: controller.signal });
      if (epoch !== this.#loadEpoch || session !== this.session) return this.references;
      this.#references = sessionAttachmentCollection(payload);
      this.#loadState = 'ready';
      this.#loadController = null;
      this.#render();
      this.#emitChange();
      return this.references;
    } catch (error) {
      if (error?.name === 'AbortError' || epoch !== this.#loadEpoch) return this.references;
      this.#loadState = 'error';
      this.#loadError = error?.message || 'Could not load session context.';
      this.#loadController = null;
      this.#render();
      return this.references;
    }
  }

  /**
   * Upload files as one-turn context. The per-reference scope control can make
   * any successful upload persistent for the Session.
   */
  async upload(files) {
    if (!this.session || this.disabled) return [];
    const accepted = [];
    for (const file of Array.from(files || [])) {
      if (!(file instanceof File)) continue;
      const uploadLimit = file.type?.toLowerCase().startsWith('image/')
        ? Math.min(this.maxUploadBytes, DEFAULT_MAX_IMAGE_UPLOAD_BYTES)
        : this.maxUploadBytes;
      if (file.size > uploadLimit) {
        this.#notify('File is too large', `${file.name} is ${formatAttachmentBytes(file.size)}; the limit is ${formatAttachmentBytes(uploadLimit)}.`, 'err');
        continue;
      }
      accepted.push({
        id: globalThis.crypto?.randomUUID?.() || `upload-${Date.now()}-${Math.random().toString(16).slice(2)}`,
        file, status: 'queued', progress: null, error: '', xhr: null, cancelled: false,
      });
    }
    if (!accepted.length) return [];

    this.#uploads.push(...accepted);
    this.#render();
    const results = await Promise.allSettled(accepted.map((upload) => this.#runUpload(upload)));
    const succeeded = results.filter((result) => result.status === 'fulfilled').length;
    this.#uploads = this.#uploads.filter((upload) => upload.status === 'error' && !upload.cancelled);
    this.#render();
    if (succeeded) {
      await this.refresh();
      this.#announce(`${succeeded} ${succeeded === 1 ? 'file' : 'files'} added to this turn.`);
    }
    return results;
  }

  /**
   * Hide accepted one-turn references immediately, then reconcile with the
   * server. Consumption itself belongs to the server's atomic turn-begin
   * transaction; the browser never tries to create that state independently.
   * Session-scoped references intentionally remain selected for later turns.
   */
  async consumeAccepted(ids) {
    const accepted = new Set(Array.from(ids || [], String));
    const consumable = this.#references.filter((reference) =>
      accepted.has(reference.id) && reference.scope === 'this_turn' && !reference.consumed,
    );
    if (!consumable.length) return [];
    const consumed = consumable.map((reference) => reference.id);
    const done = new Set(consumed);
    this.#references = this.#references.filter((reference) => !done.has(reference.id));
    this.#render();
    this.#emitChange();
    await this.refresh();
    return consumed;
  }

  #resetForSession() {
    ++this.#loadEpoch;
    this.#loadController?.abort();
    this.#loadController = null;
    for (const upload of this.#uploads) {
      upload.cancelled = true;
      upload.xhr?.abort();
    }
    this.#uploads = [];
    this.#references = [];
    this.#busyReferences.clear();
    this.#loadState = 'idle';
    this.#loadError = '';
    this.#dragDepth = 0;
    this.removeAttribute('dragging');
    this.#closeDialog(false);
    this.#render();
    this.#emitChange();
  }

  async #request(url, options = {}) {
    const response = await fetch(url, options);
    const text = await response.text();
    let body = {};
    if (text) {
      try { body = JSON.parse(text); } catch { body = { message: text.slice(0, 500) }; }
    }
    if (!response.ok) throw new Error(responseError(body, response.status));
    if (body?.error) throw new Error(responseError(body, response.status));
    return body;
  }

  #runUpload(upload) {
    const session = this.session;
    upload.status = 'uploading';
    upload.progress = null;
    upload.error = '';
    upload.cancelled = false;
    this.#render();

    return new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      let settled = false;
      upload.xhr = xhr;
      xhr.open('POST', `${sessionAttachmentsUrl(session)}?scope=this_turn`);
      xhr.timeout = 5 * 60 * 1000;
      xhr.upload.addEventListener('progress', (event) => {
        upload.progress = event.lengthComputable && event.total > 0
          ? Math.min(100, Math.round((event.loaded / event.total) * 100))
          : null;
        this.#renderUploads();
      });

      const fail = (message) => {
        if (settled) return;
        settled = true;
        upload.xhr = null;
        if (upload.cancelled) {
          upload.status = 'cancelled';
          reject(new DOMException('Upload cancelled', 'AbortError'));
          return;
        }
        upload.status = 'error';
        upload.error = message || 'Upload failed.';
        this.#notify('Upload failed', `${upload.file.name}: ${upload.error}`, 'err');
        this.#render();
        reject(new Error(upload.error));
      };

      xhr.addEventListener('load', () => {
        if (settled) return;
        if (xhr.status < 200 || xhr.status >= 300) {
          let body = {};
          try { body = JSON.parse(xhr.responseText || '{}'); } catch {}
          fail(responseError(body, xhr.status));
          return;
        }
        let body = {};
        try { body = JSON.parse(xhr.responseText || '{}'); } catch {}
        if (body?.error) {
          fail(responseError(body, xhr.status));
          return;
        }
        settled = true;
        upload.xhr = null;
        if (session !== this.session) {
          upload.status = 'cancelled';
          reject(new DOMException('Session changed during upload', 'AbortError'));
          return;
        }
        upload.status = 'complete';
        upload.progress = 100;
        this.#render();
        resolve(xhr.responseText);
      });
      xhr.addEventListener('error', () => fail('The connection failed while uploading.'));
      xhr.addEventListener('timeout', () => fail('The upload timed out.'));
      xhr.addEventListener('abort', () => fail('Upload cancelled.'));

      const form = new FormData();
      form.append('file', upload.file, upload.file.name);
      xhr.send(form);
    });
  }

  async #retryUpload(id) {
    const upload = this.#uploads.find((candidate) => candidate.id === id);
    if (!upload || this.disabled) return;
    try {
      await this.#runUpload(upload);
      this.#uploads = this.#uploads.filter((candidate) => candidate !== upload);
      await this.refresh();
      this.#announce(`${upload.file.name} added to this turn.`);
    } catch {}
    this.#render();
  }

  #cancelUpload(id) {
    const upload = this.#uploads.find((candidate) => candidate.id === id);
    if (!upload) return;
    upload.cancelled = true;
    if (upload.xhr) upload.xhr.abort();
    else this.#uploads = this.#uploads.filter((candidate) => candidate !== upload);
    this.#render();
  }

  async #toggleScope(id) {
    const reference = this.#references.find((candidate) => candidate.id === id);
    if (!reference || this.disabled || this.#busyReferences.has(id)) return;
    const session = this.session;
    const nextScope = reference.scope === 'session' ? 'this_turn' : 'session';
    this.#busyReferences.add(id);
    this.#render();
    try {
      const payload = await this.#request(sessionAttachmentsUrl(this.session, id), {
        method: 'PATCH', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ scope: nextScope }),
      });
      if (session !== this.session || !this.#references.includes(reference)) return;
      const returned = normalizeSessionAttachment(payload?.attachment ?? payload?.reference ?? payload);
      reference.scope = returned.id ? returned.scope : nextScope;
      reference.raw = returned.id ? returned.raw : reference.raw;
      this.#emitChange();
      this.#announce(reference.scope === 'session'
        ? `${reference.name} will stay in Session context.`
        : `${reference.name} will be used for the next turn only.`);
    } catch (error) {
      this.#notify('Could not change context scope', error?.message || '', 'err');
    } finally {
      this.#busyReferences.delete(id);
      this.#render();
    }
  }

  async #removeReference(id) {
    const reference = this.#references.find((candidate) => candidate.id === id);
    if (!reference || this.disabled || this.#busyReferences.has(id)) return;
    const session = this.session;
    this.#busyReferences.add(id);
    this.#render();
    try {
      await this.#request(sessionAttachmentsUrl(this.session, id), { method: 'DELETE' });
      if (session !== this.session || !this.#references.includes(reference)) return;
      this.#references = this.#references.filter((candidate) => candidate.id !== id);
      if (this.#dialog?.reference.id === id) this.#closeDialog();
      this.#emitChange();
      this.#announce(`${reference.name} removed from context.`);
    } catch (error) {
      this.#notify('Could not remove context', error?.message || '', 'err');
    } finally {
      this.#busyReferences.delete(id);
      this.#render();
    }
  }

  #render() {
    if (!this.#root) return;
    const choose = this.#root.querySelector('[data-action="choose"]');
    choose.disabled = this.disabled || !this.session;
    this.#renderChips();
    this.#renderState();
    this.#renderUploads();
    this.#renderDialog();
  }

  #renderChips() {
    const host = this.#root.querySelector('.chips');
    host.replaceChildren();
    for (const reference of this.#references) {
      const busy = this.#busyReferences.has(reference.id);
      const chip = document.createElement('div');
      chip.className = `chip${busy ? ' busy' : ''}`;
      chip.dataset.referenceId = reference.id;
      chip.setAttribute('role', 'listitem');

      const file = document.createElement('button');
      file.type = 'button';
      file.className = 'file';
      file.dataset.action = 'preview';
      file.dataset.referenceId = reference.id;
      file.disabled = busy;
      file.setAttribute('aria-label', `Preview ${reference.name}`);
      const name = document.createElement('span');
      name.className = 'file-name';
      name.textContent = reference.name;
      const size = document.createElement('span');
      size.className = 'file-size';
      size.textContent = formatAttachmentBytes(reference.sizeBytes);
      file.append(name, size);

      const scope = document.createElement('button');
      scope.type = 'button';
      scope.className = `scope${reference.scope === 'session' ? ' session' : ''}`;
      scope.dataset.action = 'toggle-scope';
      scope.dataset.referenceId = reference.id;
      scope.disabled = busy || this.disabled;
      scope.setAttribute('aria-pressed', reference.scope === 'session' ? 'true' : 'false');
      scope.setAttribute('aria-label', reference.scope === 'session'
        ? `${reference.name} stays in Session context; change to next turn only`
        : `${reference.name} is for the next turn; keep in Session context`);
      scope.title = reference.scope === 'session'
        ? 'Used in every turn until removed'
        : 'Used once, after the server accepts the next turn';
      scope.textContent = reference.scope === 'session' ? 'Session' : 'Once';

      const remove = document.createElement('button');
      remove.type = 'button';
      remove.className = 'remove';
      remove.dataset.action = 'remove';
      remove.dataset.referenceId = reference.id;
      remove.disabled = busy || this.disabled;
      remove.setAttribute('aria-label', `Remove ${reference.name} from context`);
      remove.title = 'Remove from context';
      remove.textContent = '×';
      chip.append(file, scope, remove);
      host.append(chip);
    }

  }

  #renderState() {
    const state = this.#root.querySelector('[data-role="state"]');
    state.className = 'state';
    state.replaceChildren();
    state.hidden = false;
    if (!this.session) {
      state.textContent = 'Open a Session to attach context.';
    } else if (this.#loadState === 'loading') {
      state.textContent = 'Loading Session context…';
    } else if (this.#loadState === 'error') {
      state.classList.add('error');
      const message = document.createElement('span');
      message.textContent = this.#loadError || 'Could not load Session context.';
      const retry = document.createElement('button');
      retry.type = 'button';
      retry.className = 'retry';
      retry.dataset.action = 'refresh';
      retry.textContent = 'Retry';
      state.append(message, retry);
    } else {
      state.hidden = true;
    }
  }

  #renderUploads() {
    const host = this.#root.querySelector('.uploads');
    host.replaceChildren();
    for (const upload of this.#uploads) {
      const row = document.createElement('div');
      row.className = 'upload';
      const name = document.createElement('div');
      name.className = 'upload-name';
      name.textContent = upload.file.name;
      name.title = upload.file.name;
      const progress = document.createElement('progress');
      progress.max = 100;
      if (upload.progress != null) progress.value = upload.progress;
      progress.setAttribute('aria-label', `Uploading ${upload.file.name}`);
      const actions = document.createElement('div');
      actions.className = 'upload-actions';
      const status = document.createElement('span');
      status.className = `upload-status${upload.status === 'error' ? ' error' : ''}`;
      status.textContent = upload.status === 'error'
        ? upload.error
        : upload.status === 'complete'
          ? 'Complete'
          : upload.progress == null ? 'Uploading…' : `${upload.progress}%`;
      actions.append(status);
      if (upload.status === 'error') {
        const retry = document.createElement('button');
        retry.type = 'button'; retry.className = 'upload-action'; retry.dataset.action = 'retry-upload';
        retry.dataset.uploadId = upload.id; retry.textContent = 'Retry';
        actions.append(retry);
      } else if (upload.status !== 'complete') {
        const cancel = document.createElement('button');
        cancel.type = 'button'; cancel.className = 'upload-action'; cancel.dataset.action = 'cancel-upload';
        cancel.dataset.uploadId = upload.id; cancel.textContent = 'Cancel';
        actions.append(cancel);
      }
      row.append(name, progress, actions);
      host.append(row);
    }
  }

  #openDialog(reference) {
    this.#dialog?.controller?.abort();
    const active = deepActiveElement();
    this.#dialog = {
      reference,
      kind: sessionAttachmentPreviewKind(reference),
      restoreFocus: active instanceof HTMLElement ? active : null,
      controller: new AbortController(),
      focused: false,
      loading: false,
      text: '',
      truncated: false,
      error: '',
      epoch: ++this.#dialogEpoch,
    };
    this.#renderDialog();
    if (this.#dialog.kind === 'text') void this.#loadTextPreview(this.#dialog);
  }

  #closeDialog(restore = true) {
    const dialog = this.#dialog;
    if (!dialog) return;
    dialog.controller?.abort();
    this.#dialog = null;
    this.#renderDialog();
    if (restore && dialog.restoreFocus?.isConnected) queueMicrotask(() => dialog.restoreFocus.focus());
  }

  #renderDialog() {
    const host = this.#root.querySelector('.dialog-host');
    host.replaceChildren();
    const state = this.#dialog;
    if (!state) return;
    const reference = state.reference;
    const contentUrl = sessionAttachmentsUrl(this.session, reference.id, true);

    const overlay = document.createElement('div');
    overlay.className = 'overlay';
    overlay.dataset.action = 'close-backdrop';
    const dialog = document.createElement('section');
    dialog.className = 'dialog';
    dialog.setAttribute('role', 'dialog');
    dialog.setAttribute('aria-modal', 'true');
    dialog.setAttribute('aria-labelledby', 'session-context-dialog-title');
    dialog.tabIndex = -1;
    const head = document.createElement('header');
    head.className = 'dialog-head';
    const title = document.createElement('h2');
    title.className = 'dialog-title';
    title.id = 'session-context-dialog-title';
    title.textContent = reference.name;
    const close = document.createElement('button');
    close.type = 'button'; close.className = 'dialog-close'; close.dataset.action = 'close-dialog';
    close.setAttribute('aria-label', 'Close attachment preview'); close.textContent = '×';
    head.append(title, close);

    const preview = document.createElement('div');
    preview.className = 'preview';
    if (state.kind === 'image') {
      const image = document.createElement('img');
      image.src = contentUrl;
      image.alt = `Preview of ${reference.name}`;
      image.addEventListener('error', () => {
        preview.replaceChildren(this.#previewMessage('This image could not be previewed. Download it to inspect it.', true));
      }, { once: true });
      preview.append(image);
    } else if (state.kind === 'pdf') {
      const frame = document.createElement('iframe');
      frame.src = contentUrl;
      frame.title = `Preview of ${reference.name}`;
      // User-provided documents never receive script, navigation, form, popup,
      // or same-origin privileges inside the workbench.
      frame.setAttribute('sandbox', '');
      preview.append(frame);
    } else if (state.kind === 'text') {
      if (state.error) preview.append(this.#previewMessage(state.error, true));
      else if (state.loading) preview.append(this.#previewMessage('Loading text preview…'));
      else {
        const pre = document.createElement('pre');
        pre.textContent = state.text;
        preview.append(pre);
      }
    } else {
      preview.append(this.#previewMessage('A safe inline preview is not available for this file type. Download the original to inspect it.'));
    }

    const foot = document.createElement('footer');
    foot.className = 'dialog-foot';
    const meta = document.createElement('span');
    meta.className = 'dialog-meta';
    meta.textContent = `${reference.mimeType} · ${formatAttachmentBytes(reference.sizeBytes)}${state.truncated ? ' · preview truncated' : ''}`;
    const download = document.createElement('a');
    download.className = 'download';
    download.href = contentUrl;
    download.download = safeDownloadName(reference.name);
    download.rel = 'noopener';
    download.textContent = 'Download';
    foot.append(meta, download);
    dialog.append(head, preview, foot);
    overlay.append(dialog);
    host.append(overlay);

    queueMicrotask(() => {
      if (this.#dialog !== state) return;
      const currentDialog = this.#root.querySelector('.dialog');
      const active = this.#root.activeElement;
      if (!state.focused || !currentDialog?.contains(active)) {
        (this.#root.querySelector('[data-action="close-dialog"]') || currentDialog)?.focus();
        state.focused = true;
      }
    });
  }

  #previewMessage(text, error = false) {
    const message = document.createElement('p');
    message.className = `preview-message${error ? ' error' : ''}`;
    message.textContent = text;
    return message;
  }

  async #loadTextPreview(state) {
    state.loading = true;
    this.#renderDialog();
    try {
      const response = await fetch(sessionAttachmentsUrl(this.session, state.reference.id, true), {
        signal: state.controller.signal,
      });
      if (!response.ok) throw new Error(`Preview failed (HTTP ${response.status})`);
      const reader = response.body?.getReader();
      if (!reader) throw new Error('This browser cannot stream the preview safely.');
      const chunks = [];
      let total = 0;
      let truncated = false;
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        if (!value?.length) continue;
        const remaining = TEXT_PREVIEW_LIMIT - total;
        if (remaining <= 0 || value.length > remaining) {
          if (remaining > 0) chunks.push(value.slice(0, remaining));
          total += Math.max(remaining, 0);
          truncated = true;
          await reader.cancel();
          break;
        }
        chunks.push(value);
        total += value.length;
      }
      if (this.#dialog !== state || state.epoch !== this.#dialogEpoch) return;
      const bytes = new Uint8Array(total);
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
      state.text = new TextDecoder('utf-8', { fatal: false }).decode(bytes);
      state.truncated = truncated;
      state.loading = false;
      this.#renderDialog();
    } catch (error) {
      if (error?.name === 'AbortError' || this.#dialog !== state) return;
      state.error = error?.message || 'The text preview could not be loaded.';
      state.loading = false;
      this.#renderDialog();
    }
  }

  #onClick(event) {
    const action = event.target.closest('[data-action]');
    if (!action) return;
    const kind = action.dataset.action;
    if (kind === 'choose') this.#root.querySelector('.file-input').click();
    else if (kind === 'refresh') void this.refresh();
    else if (kind === 'preview') {
      const reference = this.#references.find((candidate) => candidate.id === action.dataset.referenceId);
      if (reference) this.#openDialog(reference);
    } else if (kind === 'toggle-scope') void this.#toggleScope(action.dataset.referenceId);
    else if (kind === 'remove') void this.#removeReference(action.dataset.referenceId);
    else if (kind === 'retry-upload') void this.#retryUpload(action.dataset.uploadId);
    else if (kind === 'cancel-upload') this.#cancelUpload(action.dataset.uploadId);
    else if (kind === 'close-dialog') this.#closeDialog();
    else if (kind === 'close-backdrop' && event.target === action) this.#closeDialog();
  }

  #onChange(event) {
    if (!event.target.matches('.file-input')) return;
    const files = Array.from(event.target.files || []);
    event.target.value = '';
    void this.upload(files);
  }

  #onKeyDown(event) {
    if (!this.#dialog) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      this.#closeDialog();
      return;
    }
    if (event.key !== 'Tab') return;
    const dialog = this.#root.querySelector('.dialog');
    const focusable = dialog ? Array.from(dialog.querySelectorAll(FOCUSABLE)) : [];
    if (!focusable.length) { event.preventDefault(); dialog?.focus(); return; }
    const current = event.composedPath()[0];
    const at = focusable.indexOf(current);
    if (event.shiftKey && at <= 0) {
      event.preventDefault();
      focusable[focusable.length - 1].focus();
    } else if (!event.shiftKey && (at < 0 || at === focusable.length - 1)) {
      event.preventDefault();
      focusable[0].focus();
    }
  }

  #hasFiles(event) {
    return Array.from(event.dataTransfer?.types || []).includes('Files');
  }

  #onDragEnter(event) {
    if (!this.#hasFiles(event) || this.disabled || !this.session) return;
    event.preventDefault();
    this.#dragDepth += 1;
    this.setAttribute('dragging', '');
  }

  #onDragOver(event) {
    if (!this.#hasFiles(event) || this.disabled || !this.session) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy';
  }

  #onDragLeave(event) {
    if (!this.hasAttribute('dragging')) return;
    event.preventDefault();
    this.#dragDepth = Math.max(0, this.#dragDepth - 1);
    if (!this.#dragDepth) this.removeAttribute('dragging');
  }

  #onDrop(event) {
    if (!this.#hasFiles(event) || this.disabled || !this.session) return;
    event.preventDefault();
    this.#dragDepth = 0;
    this.removeAttribute('dragging');
    void this.upload(event.dataTransfer?.files || []);
  }

  #emitChange() {
    this.dispatchEvent(new CustomEvent('context-change', {
      detail: {
        session: this.session,
        references: this.references,
        selectedReferenceIds: this.selectedReferenceIds(),
      },
      bubbles: true,
      composed: true,
    }));
  }

  #notify(title, body = '', kind = 'info') {
    this.dispatchEvent(new CustomEvent('notify', {
      detail: { title, body, kind }, bubbles: true, composed: true,
    }));
  }

  #announce(message) {
    const announcer = this.#root.querySelector('[data-role="announcer"]');
    announcer.textContent = '';
    queueMicrotask(() => { announcer.textContent = message; });
  }
}

function deepActiveElement() {
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  return active;
}

if (globalThis.customElements && !customElements.get('ax-session-context')) {
  customElements.define('ax-session-context', AxSessionContext);
}
