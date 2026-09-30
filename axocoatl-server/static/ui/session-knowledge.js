import { adopt } from './sheets.js';

const KINDS = ['decision', 'architecture', 'convention', 'finding', 'pitfall', 'note'];
const RELATIONS = ['supports', 'depends_on', 'supersedes', 'related', 'used_by'];
const copy = value => structuredClone(value);
const item = (tag, text, attrs = {}) => {
  const node = document.createElement(tag);
  if (text !== null) node.textContent = text;
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  return node;
};
const action = (text, callback, attrs = {}) => {
  const node = item('button', text, {type: 'button', ...attrs});
  node.onclick = callback; return node;
};
const CSS = `
:host{display:inline-block;min-width:0;font:var(--fs-body,13px)/1.5 var(--font-sans);color:var(--text)}
*{box-sizing:border-box}button,input,textarea,select{font:inherit;color:inherit}
button,a.download{border:1px solid var(--border);border-radius:var(--r-md,6px);background:var(--bg-2);padding:6px 10px;cursor:pointer;color:var(--text);text-decoration:none}
button:hover:not(:disabled){border-color:var(--accent);color:var(--accent)}button:disabled{opacity:.5;cursor:default}
button:focus-visible,input:focus-visible,textarea:focus-visible,select:focus-visible,a:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
.open{margin:4px 0;padding:6px 10px;font-size:var(--fs-sm,13px)}dialog{padding:0;width:min(1100px,96vw);height:min(820px,92vh);max-width:96vw;max-height:94vh;border:1px solid var(--border-strong);border-radius:var(--r-lg,12px);background:var(--panel);color:var(--text);box-shadow:var(--shadow-lg);overflow:hidden}
dialog::backdrop{background:rgba(0,0,0,.5)}.frame{height:100%;display:flex;flex-direction:column;min-height:0}header{display:flex;align-items:center;gap:12px;padding:16px 20px;border-bottom:1px solid var(--border)}header div{flex:1;min-width:0}h2,h3,p{margin:0}h2{font-size:18px}h3{font-size:15px}.sub,.meta{color:var(--muted);font-size:12px;overflow-wrap:anywhere}
.tools{display:flex;flex-wrap:wrap;gap:8px;padding:12px 20px;border-bottom:1px solid var(--border)}.search{flex:1;min-width:150px}input,textarea,select{padding:7px 8px;border:1px solid var(--border);border-radius:5px;background:var(--bg);max-width:100%;min-width:0}input{width:100%}.tools button[aria-pressed=true]{background:var(--panel-2);border-color:var(--accent);color:var(--accent)}
.status{padding:8px 20px;color:var(--muted);font-size:12px;white-space:pre-wrap;overflow-wrap:anywhere}.status.error{color:var(--err)}.status:empty{display:none}.body{display:grid;grid-template-columns:minmax(180px,270px) minmax(0,1fr);flex:1;min-height:0}.list{overflow:auto;border-right:1px solid var(--border);padding:8px}.list:empty::before{content:'No notes match this search.';display:block;color:var(--muted);padding:12px}.note-row{display:block;width:100%;text-align:left;margin-bottom:5px;padding:10px;background:transparent}.note-row[aria-current=true]{background:var(--panel-2);border-color:var(--accent)}.note-row strong,.note-row small{display:block;overflow-wrap:anywhere}.note-row small{color:var(--muted);font-size:11px}.detail{min-width:0;overflow:auto;padding:20px}.detail>*+*{margin-top:14px}.actions{display:flex;gap:8px;flex-wrap:wrap}.badge{display:inline-block;padding:2px 7px;border:1px solid var(--border);border-radius:20px;font-size:11px}.badge.stale{color:var(--warn);border-color:var(--warn)}.badge.current{color:var(--accent)}.markdown{white-space:pre-wrap;overflow-wrap:anywhere;font:13px/1.65 var(--font-mono);margin:0;padding:16px;background:var(--bg-2);border:1px solid var(--border);border-radius:6px}ul{padding-left:18px}li+li{margin-top:7px}.inline-link{padding:1px 4px;border:0;background:transparent;text-decoration:underline;text-align:left;overflow-wrap:anywhere;max-width:100%}
.form{display:flex;flex-direction:column;gap:14px}.form label{display:flex;flex-direction:column;gap:4px}.form textarea{width:100%;min-height:190px;font:13px/1.6 var(--font-mono);resize:vertical}.form .title-kind{display:grid;grid-template-columns:minmax(0,1fr) 145px;gap:10px}.form .rows{display:flex;flex-direction:column;gap:6px}.link-row{display:grid;grid-template-columns:125px minmax(0,1fr) auto;gap:6px}.source-row{display:grid;grid-template-columns:minmax(0,1fr) minmax(0,1fr) auto;gap:6px}.source-row .symbol{grid-column:1/3}.form fieldset{min-width:0;border:1px solid var(--border);border-radius:6px;padding:10px}.form legend{font-size:12px;color:var(--muted)}.form fieldset>button{margin-top:8px}.primary{background:var(--accent);color:var(--bg)}
.wide{grid-column:1/-1;overflow:auto;padding:20px}.proposal{padding:16px;border:1px solid var(--border);border-radius:8px;margin-bottom:12px}.proposal>*+*{margin-top:10px}.proposal details pre{max-height:240px;overflow:auto}.graph-wrap{height:100%;min-height:300px;position:relative;grid-column:1/-1}.graph-wrap ax-lattice{display:block;height:100%;--ax-node-bg:var(--panel);--ax-node-fg:var(--text);--ax-node-border:var(--border-strong);--ax-edge-color:var(--muted);--ax-edge-label-color:var(--text)}.graph-wrap ax-node{width:190px;font-size:12px}.graph-wrap ax-node strong{display:block;max-width:165px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.graph-tools{position:absolute;z-index:2;right:10px;top:10px;display:flex;gap:5px}.code-file{padding:12px 0;border-bottom:1px solid var(--border)}.code-file>*+*{margin-top:7px}.symbols{display:flex;gap:6px;flex-wrap:wrap}.empty{color:var(--muted);padding:15px 0}.provenance{white-space:pre-wrap;overflow-wrap:anywhere;font-size:12px;color:var(--muted)}[hidden]{display:none!important}
@media(max-width:650px){dialog{width:98vw;max-width:98vw;height:95vh;max-height:95vh}header,.tools{padding:10px}.body{grid-template-columns:1fr;grid-template-rows:minmax(100px,28%) minmax(0,1fr)}.body.single{grid-template-rows:1fr}.list{border-right:0;border-bottom:1px solid var(--border)}.detail,.wide{padding:12px}.form .title-kind{grid-template-columns:1fr}.tools{gap:5px}.tools button,a.download{padding:5px 7px;font-size:12px}.source-row{grid-template-columns:minmax(0,1fr) auto}.source-row .sha{grid-column:1/2}.source-row .symbol{grid-column:1/2}.source-row button{grid-column:2;grid-row:1/4}.link-row{grid-template-columns:110px minmax(0,1fr) auto}.graph-wrap{min-height:0}}
`;

/** Session-scoped access to workspace-owned versioned knowledge. Text is never interpreted as HTML. */
export class SessionKnowledge extends HTMLElement {
  static observedAttributes = ['session'];
  constructor() {
    super(); this.attachShadow({mode:'open'}); this.epoch = 0; this.busy = false; this.view = null; this.tab = 'notes'; this.selected = null; this.draft = null;
    this.shadowRoot.innerHTML = `<button class="open" type="button" disabled>Knowledge</button><dialog aria-labelledby="knowledge-title"><div class="frame"><header><div><h2 id="knowledge-title">Workspace knowledge</h2><p class="sub">Notes, code, and the evidence behind them.</p></div><button class="close" type="button" aria-label="Close knowledge">Close</button></header><div class="tools"><input class="search" type="search" aria-label="Search knowledge and code" placeholder="Search notes and code…"><button class="refresh" type="button">Refresh</button><button class="new" type="button">New note</button><a class="download">Export Markdown</a></div><nav class="tools" aria-label="Knowledge views"><button data-tab="notes" type="button">Notes</button><button data-tab="graph" type="button">Graph</button><button data-tab="code" type="button">Code index</button><button data-tab="proposals" type="button">Proposals</button></nav><p class="status" role="status" aria-live="polite"></p><div class="body"></div></div></dialog>`;
    void adopt(this.shadowRoot, CSS, ['/ui/tokens.css']);
    this.q('.open').onclick = () => void this.open();
    this.q('.close').onclick = () => this.close();
    this.q('dialog').addEventListener('cancel', event => { event.preventDefault(); this.close(); });
    this.q('.refresh').onclick = () => void this.load();
    this.q('.new').onclick = () => this.edit(null);
    const importFile = item('input',null,{type:'file',accept:'.md,.markdown,text/markdown',hidden:'','aria-label':'Import knowledge Markdown'});
    const importButton = action('Import Markdown',()=>importFile.click(),{class:'import'});
    this.q('.tools').insertBefore(importButton,this.q('.download'));this.q('.tools').append(importFile);
    importFile.onchange = () => {const file=importFile.files?.[0];importFile.value='';if(file)void this.importMarkdown(file);};
    this.q('.search').addEventListener('input', () => { clearTimeout(this.searchTimer); this.searchTimer = setTimeout(() => void this.load(), 220); });
    for (const button of this.shadowRoot.querySelectorAll('[data-tab]')) button.onclick = () => { this.tab = button.dataset.tab; this.render(); };
  }
  q(selector) { return this.shadowRoot.querySelector(selector); }
  get session() { return this.getAttribute('session') || ''; }
  attributeChangedCallback(name, old, value) {
    if (old === value) return;
    this.epoch++; this.read?.abort(); clearTimeout(this.searchTimer); this.close(); this.view = null; this.selected = null; this.draft = null; this.busy = false;
    this.q('.search').value = ''; this.q('.body').replaceChildren(); this.status(''); this.q('.open').disabled = !value;
    this.q('.download').href = this.base() + '/export';
  }
  disconnectedCallback() { this.read?.abort(); clearTimeout(this.searchTimer); }
  base() { return `/api/sessions/${encodeURIComponent(this.session)}/knowledge`; }
  emit(name, detail) { this.dispatchEvent(new CustomEvent(name, {bubbles:true,composed:true,detail:{session_id:this.session,...detail}})); }
  status(text, error = false) { this.q('.status').textContent = text; this.q('.status').classList.toggle('error', error); }
  async open(noteId = null) { if (!this.session) return; if (noteId) {this.selected = noteId; this.tab = 'notes';} if (!this.q('dialog').open) this.q('dialog').showModal(); await this.load(); }
  close() { this.q('dialog').close(); }
  async request(path, options = {}) {
    const response = await fetch(path, options); const payload = await response.json().catch(() => null);
    if (!response.ok) { const error = new Error(payload?.error || payload?.message || `Knowledge request failed (${response.status})`); error.status = response.status; throw error; }
    return payload;
  }
  async load() {
    if (!this.session || this.busy) return;
    const epoch = ++this.epoch, session = this.session; this.read?.abort(); this.read = new AbortController(); this.status('Loading workspace knowledge…');
    try {
      const view = await this.request(`${this.base()}?q=${encodeURIComponent(this.q('.search').value)}`, {signal:this.read.signal});
      if (epoch !== this.epoch || session !== this.session) return;
      this.view = view; this.status(''); this.render();
    } catch (error) { if (epoch === this.epoch && error.name !== 'AbortError') this.status(error.message, true); }
  }
  async mutate(suffix, body, callback, method = 'POST', success = 'Saved.') {
    if (this.busy || !this.session) return; const session = this.session, epoch = ++this.epoch; this.read?.abort(); this.busy = true; this.controls(); this.status('Saving…');
    try {
      const result = await this.request(this.base() + suffix, {method,headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});
      if (epoch !== this.epoch || session !== this.session) return;
      callback?.(result); this.status(success);
    } catch (error) { if (epoch === this.epoch) this.status(`${error.message}${error.status === 409 ? '\nThe recorded revision changed. Refresh and review the current note before saving again.' : '\nYour draft is retained. Refresh to check whether the request was recorded before retrying.'}`, true); }
    finally { if (epoch === this.epoch) {this.busy = false; this.controls();} }
  }
  controls() { for (const button of this.shadowRoot.querySelectorAll('.tools button,.detail button,.wide button,.list button,.form input,.form textarea,.form select')) button.disabled = this.busy; this.q('.new').disabled = this.q('.import').disabled = this.busy || Boolean(this.draft); }
  async importMarkdown(file) {
    if(this.busy||this.draft||!this.session)return;
    if(file.size>256*1024){this.status('Choose a Markdown note smaller than 256 KiB.',true);return;}
    const session=this.session;
    try{
      const markdown=await file.text();if(session!==this.session)return;
      await this.mutate('/import-preview',{markdown},edit=>{
        this.draft={id:edit.id||`note-${crypto.randomUUID()}`,revision:edit.expected_revision,title:edit.title,body:edit.body,kind:edit.kind,links:copy(edit.links||[]),sources:copy(edit.sources||[])};
        this.tab='notes';this.render();
      },'POST','Import preview ready. Review the Markdown and recorded revision, then Save note.');
    }catch(error){if(session===this.session)this.status(`Could not read the selected Markdown: ${error.message}`,true);}
  }
  render() {
    if (!this.view) return;
    for (const button of this.shadowRoot.querySelectorAll('[data-tab]')) button.setAttribute('aria-pressed', String(button.dataset.tab === this.tab));
    const pending = this.view.proposals.filter(proposal => proposal.status === 'pending').length;
    this.q('[data-tab=proposals]').textContent = `Proposals${pending ? ` (${pending})` : ''}`;
    const renderId = this.renderId = (this.renderId || 0) + 1;
    const body = this.q('.body'); body.replaceChildren(); body.classList.toggle('single', this.tab !== 'notes');
    if (this.tab === 'graph') void this.renderGraph(body, renderId);
    else if (this.tab === 'code') this.renderCode(body);
    else if (this.tab === 'proposals') this.renderProposals(body);
    else {
      const list = item('aside', null, {class:'list','aria-label':'Knowledge notes'}), detail = item('section', null, {class:'detail','aria-label':'Selected note'}); body.append(list, detail);
      for (const note of this.view.notes) {
        const button = action('', () => {if(this.draft){this.status('Save or cancel your draft before opening another note.');return;}this.selected = note.id; this.render();}, {class:'note-row','aria-current':String(note.id === this.selected)});
        button.append(item('strong', note.title), item('small', `${note.kind} · revision ${note.revision} · ${note.freshness}`)); list.append(button);
      }
      if (this.draft) this.renderEditor(detail);
      else {
        const note = this.view.notes.find(note => note.id === this.selected) || this.view.notes[0];
        if (note) {this.selected = note.id; this.renderNote(detail, note);} else detail.append(item('p', 'Create a note to retain a decision, finding, or convention for this workspace.', {class:'empty'}));
      }
    }
    this.controls();
  }
  renderNote(host, note) {
    host.append(item('h3', note.title), item('span', `${note.kind} · revision ${note.revision} · ${note.freshness}`, {class:`badge ${note.freshness}`}));
    if (note.freshness === 'stale') host.append(item('p', 'A recorded source has changed. Review this note against the current code before relying on it.', {class:'meta'}));
    if (['unverified','unavailable'].includes(note.freshness)) host.append(item('p', 'Source freshness has not been verified. The current source may be unavailable.', {class:'meta'}));
    if (note.freshness === 'unreferenced') host.append(item('p', 'This note has no source references to check.', {class:'meta'}));
    const actions = item('div', null, {class:'actions'});
    actions.append(action('Edit note', () => this.edit(note)), action('Attach to chat', () => void this.attach(note)), action('Investigate in chat', () => void this.attach(note, true)),item('a','Export note',{class:'download',href:`${this.base()}/export?note_id=${encodeURIComponent(note.id)}`})); host.append(actions);
    host.append(item('pre', note.body, {class:'markdown','aria-label':'Markdown note'}));
    this.renderEvidence(host, note);
    if (note.links?.length) {host.append(item('h3','Links')); const list = item('ul',null); for (const link of note.links) {const row = item('li', `${link.kind.replaceAll('_',' ')} `); row.append(this.noteLink(link.target)); list.append(row);} host.append(list);}
    if (note.backlinks?.length) {host.append(item('h3','Backlinks')); const list = item('ul',null); for (const link of note.backlinks) {const row = item('li',`${link.kind.replaceAll('_',' ')} from `); row.append(this.noteLink(link.id, link.title)); list.append(row);} host.append(list);}
  }
  noteLink(id, title) { return action(title || this.view.notes.find(note => note.id === id)?.title || id, () => {this.selected = id; this.draft = null; this.tab = 'notes'; this.q('.search').value = ''; void this.load();}, {class:'inline-link'}); }
  renderEvidence(host, note) {
    if (note.sources?.length) {
      host.append(item('h3','Sources')); const list = item('ul',null);
      for (const source of note.sources) {
        const entry = this.view.code_index?.entries?.find(entry => entry.path === source.path);
        const symbol = entry?.symbols?.find(symbol => symbol.name === source.symbol);
        const row = item('li',null); row.append(action(source.path + (source.symbol ? ` · ${source.symbol}` : ''), () => this.openSource(source.path, symbol?.line), {class:'inline-link'}), item('div',`Recorded SHA-256: ${source.sha256}`,{class:'meta'})); list.append(row);
      }
      host.append(list);
    }
    const provenance = note.provenance;
    if (provenance) {
      host.append(item('p',`Authored by ${provenance.kind}${provenance.author ? ` · ${provenance.author}` : ''}${note.acceptance?.kind ? ` · accepted by ${note.acceptance.kind}` : ''}`,{class:'meta'}));
      const activation = provenance.activation;
      if (activation?.session_id && activation?.turn_id) host.append(action(`Inspect source turn · ${activation.node_id} · generation ${activation.generation}`, () => {this.emit('knowledge-open-evidence',{activation:copy(activation)});this.close();}));
      if (provenance.evidence?.length) host.append(item('p',`Retained evidence: ${provenance.evidence.join(', ')}`,{class:'provenance'}));
    }
  }
  openSource(path, line = 1) { this.emit('knowledge-open-source',{path,line:Number.isSafeInteger(line) && line > 0 ? line : 1}); this.close(); }
  async attach(note, investigate = false) {
    await this.mutate(`/${encodeURIComponent(note.id)}/attach`, {expected_revision:note.revision}, reference => {
      this.emit('knowledge-attached',{reference,note:{id:note.id,title:note.title,revision:note.revision},instruction:investigate ? `Investigate the attached knowledge note “${note.title}” (revision ${note.revision}) against the current code. Check its sources, identify any contradictions, and propose an update with evidence.` : null}); this.close();
    });
  }
  edit(note) {
    if (this.busy) return;
    this.tab = 'notes'; this.draft = note ? copy(note) : {id:`note-${crypto.randomUUID()}`,revision:0,title:'',body:'',kind:'note',links:[],sources:[]}; this.render(); this.q('.note-title').focus();
  }
  renderEditor(host) {
    const draft = this.draft, form = item('form',null,{class:'form'});
    form.append(item('h3', draft.revision ? `Edit revision ${draft.revision}` : 'New knowledge note'));
    const heading = item('div',null,{class:'title-kind'}), titleLabel = item('label','Title'), title = item('input',null,{class:'note-title',required:'',maxlength:'240'}), kindLabel = item('label','Kind'), kind = item('select',null,{'aria-label':'Kind'});
    title.value = draft.title; title.oninput = () => {draft.title = title.value;};
    for (const value of KINDS) kind.append(item('option',value,{value})); kind.value = draft.kind; kind.onchange = () => {draft.kind = kind.value;}; titleLabel.append(title);kindLabel.append(kind);heading.append(titleLabel,kindLabel);form.append(heading);
    const bodyLabel = item('label','Markdown'), body = item('textarea',null,{required:'',spellcheck:'false'});body.value = draft.body;body.oninput = () => {draft.body = body.value;};bodyLabel.append(body);form.append(bodyLabel);
    const links = item('fieldset',null), linkRows = item('div',null,{class:'rows'});links.append(item('legend','Typed links to notes'),linkRows);
    const suggestions = item('datalist',null,{id:'knowledge-targets'});for (const note of this.view.notes) suggestions.append(item('option',note.title,{value:note.id}));links.append(suggestions);
    draft.links.forEach((link,index) => {
      const row = item('div',null,{class:'link-row'}), relation = item('select',null,{'aria-label':`Link ${index+1} kind`}), target = item('input',null,{'aria-label':`Link ${index+1} target`,list:'knowledge-targets',required:''});
      for (const value of RELATIONS) relation.append(item('option',value.replaceAll('_',' '),{value}));relation.value = link.kind;relation.onchange = () => {link.kind = relation.value;};target.value = link.target;target.oninput = () => {link.target = target.value;};
      row.append(relation,target,action('×',()=>{draft.links.splice(index,1);this.render();},{'aria-label':`Remove link ${index+1}`}));linkRows.append(row);
    });
    links.append(action('Add link',()=>{draft.links.push({kind:'related',target:''});this.render();}));form.append(links);
    const sources = item('fieldset',null), sourceRows = item('div',null,{class:'rows'});sources.append(item('legend','Source references'),sourceRows);
    const paths = item('datalist',null,{id:'knowledge-paths'});for (const entry of this.view.code_index?.entries || []) paths.append(item('option','',{value:entry.path}));sources.append(paths);
    draft.sources.forEach((source,index) => {
      const row = item('div',null,{class:'source-row'}), path = item('input',null,{'aria-label':`Source ${index+1} path`,list:'knowledge-paths',placeholder:'Repository-relative path',required:''}), sha = item('input',null,{class:'sha','aria-label':`Source ${index+1} SHA-256`,placeholder:'Recorded SHA-256',required:''}), symbol = item('input',null,{class:'symbol','aria-label':`Source ${index+1} symbol`,placeholder:'Symbol (optional)'});
      path.value = source.path;sha.value = source.sha256;symbol.value = source.symbol || '';path.oninput = () => {source.path = path.value;};path.onchange = () => {const entry=this.view.code_index?.entries?.find(entry=>entry.path===path.value);if(entry){source.sha256=entry.sha256;sha.value=entry.sha256;}};sha.oninput=()=>{source.sha256=sha.value;};symbol.oninput=()=>{source.symbol=symbol.value||null;};
      row.append(path,sha,symbol,action('×',()=>{draft.sources.splice(index,1);this.render();},{'aria-label':`Remove source ${index+1}`}));sourceRows.append(row);
    });
    sources.append(action('Add source',()=>{draft.sources.push({path:'',sha256:'',symbol:null});this.render();}),item('p','Choosing an indexed path records its indexed SHA-256. Refresh the code index to observe current files.',{class:'meta'}));form.append(sources);
    const actions=item('div',null,{class:'actions'}),save=item('button','Save note',{type:'submit',class:'primary'});actions.append(save,action('Cancel edit',()=>{this.draft=null;this.render();}));form.append(actions);
    form.onsubmit=event=>{event.preventDefault();if(!form.reportValidity())return;const edit={id:draft.id,expected_revision:draft.revision,title:draft.title,body:draft.body,kind:draft.kind,links:copy(draft.links),sources:copy(draft.sources)};void this.mutate(draft.revision?`/${encodeURIComponent(draft.id)}`:'',edit,note=>{this.draft=null;this.selected=note.id;this.view.notes=[note,...this.view.notes.filter(item=>item.id!==note.id)];this.render();},draft.revision?'PUT':'POST');};host.append(form);
  }
  renderCode(host) {
    const section=item('section',null,{class:'wide'}),index=this.view.code_index;host.append(section);
    section.append(item('h3','Code index'),item('p',`${index?.status || 'unavailable'} · ${index?.files || 0} files · ${index?.symbols || 0} symbols. Parsed definitions and imports; this is not a complete call graph.`,{class:'meta'}),action('Refresh code index',()=>void this.mutate('/index',{},view=>{this.view=view;this.render();})));
    const query=this.q('.search').value.toLocaleLowerCase();
    for(const entry of index?.entries || []){
      const matching=(entry.symbols||[]).filter(symbol=>!query||entry.path.toLocaleLowerCase().includes(query)||symbol.name.toLocaleLowerCase().includes(query));
      if(query&&!entry.path.toLocaleLowerCase().includes(query)&&!matching.length)continue;
      const file=item('section',null,{class:'code-file'});file.append(action(entry.path,()=>this.openSource(entry.path),{class:'inline-link'}),item('p',`${entry.language || 'Language unrecorded'} · ${entry.parse_status || 'Parse status unrecorded'}${entry.truncated ? ' · truncated' : ''}${entry.parser_version ? ` · parser ${entry.parser_version}` : ''}`,{class:'meta'}),item('p',`SHA-256: ${entry.sha256}`,{class:'meta'}));
      const symbols=item('div',null,{class:'symbols'});for(const symbol of matching)symbols.append(action(`${symbol.name} · ${symbol.kind} · line ${symbol.line}`,()=>this.openSource(entry.path,symbol.line)));file.append(symbols);section.append(file);
      if(entry.imports?.length){const imports=item('details',null);imports.append(item('summary',`Observed imports (${entry.imports.length}) · targets not resolved`));for(const imported of entry.imports)imports.append(action(`${imported.text} · line ${imported.line}`,()=>this.openSource(entry.path,imported.line),{class:'inline-link'}));file.append(imports);}
    }
    if(!index?.entries?.length)section.append(item('p','No code has been indexed. Refresh the code index to observe the workspace.',{class:'empty'}));
  }
  renderProposals(host) {
    const section=item('section',null,{class:'wide'});host.append(section);section.append(item('p','Review the proposed Markdown, sources, and recorded revision before accepting.',{class:'meta'}));
    for(const proposal of this.view.proposals){
      const card=item('article',null,{class:'proposal'}),note=proposal.note;card.append(item('h3',note.title),item('p',`${proposal.status} · ${note.kind} · expected revision ${proposal.expected_revision}`,{class:'meta'}),item('pre',note.body,{class:'markdown'}));
      const current=this.view.notes.find(current=>current.id===note.id);if(current){const previous=item('details',null);previous.append(item('summary',`Current note · revision ${current.revision}`),item('pre',current.body,{class:'markdown'}));card.append(previous);}
      this.renderEvidence(card,note);
      if(note.links?.length)card.append(item('p',`Proposed links: ${note.links.map(link=>`${link.kind} → ${link.target}`).join('; ')}`,{class:'meta'}));
      if(proposal.status==='pending'){
        const controls=item('div',null,{class:'actions'});controls.append(action('Accept proposal',()=>void this.mutate(`/proposals/${encodeURIComponent(proposal.id)}/accept`,{expected_revision:proposal.expected_revision},view=>{this.view=view;this.render();})),action('Reject proposal',()=>void this.mutate(`/proposals/${encodeURIComponent(proposal.id)}/reject`,{},view=>{this.view=view;this.render();})));card.append(controls);
      }
      section.append(card);
    }
    if(!this.view.proposals.length)section.append(item('p','No knowledge proposals are recorded.',{class:'empty'}));
  }
  async renderGraph(host, renderId) {
    const epoch = this.epoch, view = this.view;
    const current = () => this.isConnected && this.tab === 'graph' && this.renderId === renderId && this.epoch === epoch && this.view === view;
    host.append(item('p','Loading knowledge graph…',{class:'wide'}));
    try { await import('../lattice/index.js'); }
    catch (error) { if (current()) host.replaceChildren(item('p',`Knowledge graph unavailable: ${error.message}`,{class:'wide'})); return; }
    if (!current()) return;
    host.replaceChildren();
    const wrapper=item('div',null,{class:'graph-wrap'}),graph=item('ax-lattice',null,{mode:'view','aria-label':'Knowledge links and source evidence graph'});wrapper.append(graph);host.append(wrapper);
    const records=[],edges=[],ids=new Map(),add=(key,title,type,open)=>{if(ids.has(key))return ids.get(key);const id=`knowledge-graph-${records.length}`;ids.set(key,id);records.push({id,title,type,open});return id;};
    for(const note of this.view.notes)add(`note:${note.id}`,note.title,note.kind,()=>{if(this.draft){this.status('Save or cancel your draft before opening another note.');return;}this.selected=note.id;this.tab='notes';this.render();});
    for(const note of this.view.notes){
      const from=ids.get(`note:${note.id}`);
      for(const link of note.links||[]){const to=ids.get(`note:${link.target}`);if(to)edges.push({from,to,label:link.kind.replaceAll('_',' ')});}
      for(const source of note.sources||[]){const to=add(`code:${source.path}`,source.path,'code',()=>this.openSource(source.path));edges.push({from,to,label:'source'});}
      const activation=note.provenance?.activation;
      if(activation){const to=add(`activation:${activation.activation_id}`,`${activation.node_id} · generation ${activation.generation}`,'execution',()=>{this.emit('knowledge-open-evidence',{activation:copy(activation)});this.close();});edges.push({from,to,label:'produced by'});}
    }
    records.forEach((record,index)=>{const node=item('ax-node',null,{id:record.id,'data-x':String((index%3)*260+40),'data-y':String(Math.floor(index/3)*125+55),'data-w':'190','data-h':'68',draggable:'false','aria-label':`${record.title} · ${record.type}`});node.append(item('strong',record.title),item('small',record.type));node.addEventListener('node-click',record.open);graph.append(node);});
    graph.addEventListener('node-inspect',event=>records.find(record=>record.id===event.detail?.id)?.open());
    for(const edge of edges)graph.append(item('ax-edge',null,edge));
    const tools=item('div',null,{class:'graph-tools'});tools.append(action('Fit graph',()=>graph.fitView()),action('Read notes',()=>{this.tab='notes';this.render();}));wrapper.append(tools);
    if(!records.length)wrapper.append(item('p','Create notes and source links to build this view.',{class:'empty'}));
    requestAnimationFrame(()=>{if(graph.isConnected&&graph.clientWidth)graph.fitView();});
  }
}
customElements.define('ax-session-knowledge',SessionKnowledge);
