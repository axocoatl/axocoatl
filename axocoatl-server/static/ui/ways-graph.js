import { adopt } from './sheets.js';

// A read-only projection of the existing attempt owner or retained decision.
// Comparison navigation is the only decision action; this component never applies patches.
const node = (tag, text, className) => { const item = document.createElement(tag); if (text !== undefined) item.textContent = text; if (className) item.className = className; return item; };
const available = value => value?.state === 'available' ? value.value : null;
const complete = text => ({state: 'complete', text});
const absent = detail => ({state: 'unavailable', detail});
const stateName = value => String(value || 'unknown').replaceAll('_', ' ');
const css = `:host{display:flex;flex:1;flex-direction:column;min-width:0;background:var(--bg-2);color:var(--text);font:var(--fs-body)/var(--lh-body) var(--font-sans)}*{box-sizing:border-box}header,.actions{display:flex;align-items:center;gap:var(--sp-2);flex-wrap:wrap;padding:var(--sp-3)}h2{font-size:var(--fs-lg);margin:0;flex:1}button,select{font:inherit;color:inherit;background:var(--bg-2);border:1px solid var(--border);border-radius:var(--r-sm);padding:var(--sp-2);cursor:pointer}button:disabled{opacity:.5;cursor:default}button:focus-visible,summary:focus-visible{outline:2px solid var(--accent);outline-offset:2px}.status{padding:var(--sp-2) var(--sp-3);color:var(--muted)}ax-lattice{display:block;height:360px;min-height:300px;flex:none;background:var(--bg);border:1px solid var(--border);--ax-accent:var(--accent)}ax-node{width:220px;height:120px;padding:var(--sp-3);background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:var(--r-md)}ax-node[data-kind=decision]{border-color:var(--accent)}ax-node strong,ax-node small{display:block;overflow-wrap:anywhere;max-height:3em;overflow:hidden}ax-node small{color:var(--muted);margin:var(--sp-1) 0}.evidence{padding:var(--sp-3);overflow:auto}h3{font-size:var(--fs-body);margin:var(--sp-3) 0 var(--sp-2)}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:var(--bg);padding:var(--sp-3);max-height:340px;overflow:auto}p{overflow-wrap:anywhere}details{border:1px solid var(--border);border-radius:var(--r-sm);padding:var(--sp-2);margin:var(--sp-2) 0}summary{cursor:pointer}@media(max-width:600px){header h2{flex-basis:100%}ax-lattice{height:480px}.evidence{padding:var(--sp-2)}}`;
function textEvidence(parent, label, value) {
  parent.append(node('h3', label));
  if (!value || value.state === 'unavailable') parent.append(node('p', value?.detail || 'Not recorded'));
  else { parent.append(node('pre', value.text ?? '')); if (value.state === 'truncated') parent.append(node('small', `Truncated: ${new TextEncoder().encode(value.text || '').length} of ${value.original_bytes} bytes retained; offset ${value.offset_bytes}.`)); }
}
function usageText(usage) {
  if (!usage) return 'Usage not recorded; cost unknown.';
  const counts = usage.tokens?.usage || usage.tokens?.known_subtotal;
  if (!counts) return 'Usage not recorded; cost unknown.';
  return `${counts.input_tokens ?? 0} input / ${counts.output_tokens ?? 0} output tokens${usage.tokens.kind === 'unknown' ? ' · known subtotal; total unknown' : ''}. Cost: $${usage.cost_usd_known_subtotal ?? 0}${usage.cost_complete ? '' : ' known subtotal; total unknown'}.`;
}
export class AxWaysGraph extends HTMLElement {
  constructor() {
    super(); this.attachShadow({mode:'open'}); this.expanded = new Set(); this.inspections = new Map(); this.sequence = 0;
    this.shadowRoot.innerHTML = '<header><h2>Ways decision</h2><button class="refresh">Refresh evidence</button><button class="agents">Agent execution graph</button><button class="compare" hidden>Open comparison</button></header><div class="actions" aria-label="Graph viewport"><button class="zoom-in" aria-label="Zoom in">+</button><button class="zoom-out" aria-label="Zoom out">−</button><button class="fit">Fit graph</button></div><p class="status" role="status"></p><ax-lattice mode="view" background="dots" tabindex="0" aria-label="Ways decision and candidate subgraphs"></ax-lattice><section class="evidence" aria-label="Selected candidate evidence"></section>';
    void adopt(this.shadowRoot, css, ['/ui/tokens.css']);
    this.q('.zoom-in').onclick = () => this.q('ax-lattice').zoomIn();
    this.q('.zoom-out').onclick = () => this.q('ax-lattice').zoomOut();
    this.q('.fit').onclick = () => this.q('ax-lattice').fitView({maxZoom:1});
    this.q('.refresh').onclick = () => void this.load();
    this.q('.agents').onclick = () => this.dispatchEvent(new CustomEvent('close-ways-graph',{bubbles:true,composed:true}));
    this.q('.compare').onclick = () => { if (!this.record && this.results?.attempt_set?.id === this.setId) this.dispatchEvent(new CustomEvent('review-ways-decision',{bubbles:true,composed:true,detail:{sessionId:this.sessionId,setId:this.setId}})); };
  }
  q(selector) { return this.shadowRoot.querySelector(selector); }
  disconnectedCallback() { clearTimeout(this.timer); this.sequence++; }
  async show({sessionId, setId, record = null}) {
    clearTimeout(this.timer); this.sequence++; this.sessionId = sessionId; this.setId = setId;
    this.record = null; this.results = null; this.expanded.clear(); this.inspections.clear(); this.selected = null;
    if (record && (record.session_id !== sessionId || record.set_id !== setId)) { this.q('.status').textContent = 'Decision belongs to another Session or attempt set.'; return; }
    await import('/lattice/index.js');
    if (record) { this.record = structuredClone(record); this.render(); } else await this.load();
  }
  async request(suffix) {
    const response = await fetch(`/api/sessions/${encodeURIComponent(this.sessionId)}/${suffix}`);
    const body = await response.json(); if (!response.ok) throw new Error(body.error || `Evidence unavailable (${response.status})`); return body;
  }
  async load() {
    if (this.loading) return;
    clearTimeout(this.timer); const sequence = this.sequence; this.loading = true; this.q('.refresh').disabled = true;
    try {
      if (this.record) {
        const archive = await this.request('ways-history');
        if (sequence !== this.sequence) return;
        const record = archive.decisions?.find(item => item.decision_id === this.record.decision_id && item.set_id === this.setId && item.session_id === this.sessionId);
        if (!record) throw new Error('This retained decision is unavailable or was explicitly deleted.');
        this.record = record;
      } else {
        const results = await this.request('variants/results');
        if (sequence !== this.sequence) return;
        if (results.attempt_set?.id === this.setId && results.attempt_set.session_id === this.sessionId) this.results = results;
        else {
          const archive = await this.request('ways-history');
          if (sequence !== this.sequence) return;
          const record = archive.decisions?.find(item => item.set_id === this.setId && item.session_id === this.sessionId);
          if (!record) throw new Error('This attempt set is no longer active and its retained decision is unavailable.');
          this.record = record; this.results = null;
        }
      }
      this.render();
    } catch (error) {
      if (sequence === this.sequence) { this.q('.status').textContent = error.message; this.q('.compare').hidden = true; }
    } finally {
      if (sequence === this.sequence) { this.loading = false; this.q('.refresh').disabled = false; if (!this.record && this.isConnected) this.timer = setTimeout(() => void this.load(), 3000); }
    }
  }
  candidates() {
    if (this.record) return this.record.candidates;
    return (this.results?.attempt_set?.lanes || []).map(lane => {
      const fact = this.results.lane_states?.find(item => item.index === lane.index), usage = this.results.usage?.find(item => item.index === lane.index), output = this.results.outputs?.find(item => item.index === lane.index);
      const verdict = this.results.verdicts?.find(item => item.index === lane.index);
      return {id:{set_id:this.setId,index:lane.index},agent:lane.agent,model:lane.provider && lane.model ? {state:'available',value:{provider_id:lane.provider,model_id:lane.model}} : null,isolation:{state:'available',value:'Local Podman · independent checkout'},terminal:fact?.state || 'unknown',outcome:output ? complete(output.content) : absent('No completed output recorded.'),route:absent('Select this candidate to read its recorded Route.'),reviewable_diff:absent('Select a changed path to inspect its exact available diff.'),changed_paths:{items:[]},checks:verdict ? [{command:{state:'unavailable'},outcome:verdict.passed?'passed':'failed',exit_code:{state:'available',value:verdict.exit_code},output:complete(verdict.output),duration_ms:null}] : [],usage:usage ? {tokens:{kind:usage.token_usage_known?'measured':'unknown',[usage.token_usage_known?'usage':'known_subtotal']:{input_tokens:usage.input_tokens,output_tokens:usage.output_tokens,reasoning_tokens:usage.reasoning_tokens}},cost_usd_known_subtotal:usage.cost_usd,cost_complete:usage.cost_known} : null,failure:fact?.error};
    });
  }
  render() {
    const candidates = this.candidates(), graph = this.q('ax-lattice');
    const viewport = this.renderedSet === this.setId ? graph.getViewport?.() : null;
    this.renderedSet = this.setId; graph.replaceChildren(); graph.mode = 'view';
    const choice = this.record?.human_decision?.choice;
    const title = choice ? choice.kind === 'keep' ? `Kept Attempt ${choice.patch.candidate.index + 1}` : 'Finished without keeping' : 'Unresolved Ways decision';
    this.q('h2').textContent = title;
    this.q('.status').textContent = `${this.setId} · ${this.record ? 'Retained decision · read only' : stateName(this.results?.attempt_set?.state)}. Expand a candidate to inspect its Agent and evidence.`;
    this.q('.compare').hidden = Boolean(this.record);
    this.vertical = graph.clientWidth < 600;
    this.addNode('decision', title, this.record?.task?.text || this.results?.attempt_set?.task || 'Task not recorded', 20, 30, 'decision');
    let nextY = 180;
    for (const [position, candidate] of candidates.entries()) {
      const key = `${this.setId}:${candidate.id.index}`, id = `candidate-${candidate.id.index}`, expanded = this.expanded.has(key);
      const item = this.addNode(id, `Attempt ${candidate.id.index + 1}`, stateName(candidate.terminal), this.vertical ? 20 : 290, this.vertical ? nextY : 30 + position * 150, 'candidate');
      item.dataset.setId = this.setId; item.dataset.candidateIndex = String(candidate.id.index);
      const expand = node('button', `${expanded ? 'Collapse' : 'Expand'} Attempt ${candidate.id.index + 1}`); expand.setAttribute('aria-expanded', String(expanded));
      expand.onclick = event => { event.stopPropagation(); if (expanded) this.expanded.delete(key); else this.expanded.add(key); this.fitRequested = true; this.selected = candidate.id.index; this.render(); void this.inspect(candidate); };
      item.append(expand); this.edge('decision', id);
      if (expanded) {
        const model = available(candidate.model), agent = this.addNode(`agent-${candidate.id.index}`, candidate.agent || 'Agent not recorded', `${model ? `${model.provider_id} / ${model.model_id}` : 'Model not recorded'}\n${available(candidate.isolation) || 'Isolation not recorded'}`, this.vertical ? 20 : 560, this.vertical ? nextY + 150 : 30 + position * 150, 'agent');
        const inspect = node('button', `Inspect Attempt ${candidate.id.index + 1}`); inspect.onclick = event => { event.stopPropagation(); this.selected = candidate.id.index; void this.inspect(candidate); }; agent.append(inspect); this.edge(id, `agent-${candidate.id.index}`);
      }
      nextY += expanded ? 300 : 150;
    }
    if (viewport && !this.fitRequested) graph.setViewport(viewport); else requestAnimationFrame(() => { if (this.isConnected) graph.fitView({maxZoom:1}); });
    this.fitRequested = false;
    const selected = candidates.find(item => item.id.index === this.selected);
    if (selected) this.renderEvidence({...selected,...this.inspections.get(selected.id.index),outcome:selected.outcome,usage:selected.usage,checks:selected.checks}); else this.q('.evidence').replaceChildren(node('p','Expand an Attempt to inspect its evidence.'));
  }
  addNode(id, label, detail, x, y, kind) {
    const item = node('ax-node'); item.id = id; item.dataset.kind = kind; item.setAttribute('data-x', x); item.setAttribute('data-y', y); item.setAttribute('data-w', '220'); item.setAttribute('data-h', '120');
    item.append(node('strong',label),node('small',detail));
    for (const [type,position,id] of [['target',this.vertical ? 'top' : 'left','in'],['source',this.vertical ? 'bottom' : 'right','out']]) { const handle = node('ax-handle'); handle.setAttribute('type',type);handle.setAttribute('position',position);handle.setAttribute('handle-id',id);item.append(handle); }
    this.q('ax-lattice').append(item); return item;
  }
  edge(from,to) { const edge=node('ax-edge');edge.setAttribute('from',`${from}:out`);edge.setAttribute('to',`${to}:in`);this.q('ax-lattice').append(edge); }
  renderEvidence(candidate) {
    const panel=this.q('.evidence');panel.replaceChildren(node('h2',`Attempt ${candidate.id.index + 1}`),node('p',usageText(candidate.usage)));
    if(candidate.failure)panel.append(node('p',candidate.failure));
    const failure = available(candidate.failure_or_no_change_reason); if (failure) textEvidence(panel,'Failure or no change',failure);
    textEvidence(panel,'Outcome',candidate.outcome);textEvidence(panel,'Route',candidate.route);textEvidence(panel,'Diff',candidate.reviewable_diff);
    for (const [path,diff] of candidate.liveDiffs || []) { panel.append(node('h3',path),node('pre',diff)); }
    panel.append(node('p',`Changed paths: ${candidate.changed_paths?.items?.join(', ') || 'Not recorded'}`));
    const checks=node('section');checks.append(node('h3','Checks'));
    if(!candidate.checks?.length)checks.append(node('p','No check recorded.'));
    for(const check of candidate.checks || []) {const detail=node('details');detail.append(node('summary',stateName(typeof check.outcome==='string'?check.outcome:'verification rejected')));detail.append(node('p',available(check.command)||'Command not recorded'),node('p',`Exit: ${available(check.exit_code) ?? 'unknown'}`));textEvidence(detail,'Observed output',check.output);checks.append(detail);}panel.append(checks);
    const judge=this.record?.judge; textEvidence(panel,'Judge',judge ? (judge.result || available(judge)?.result) : this.results?.judgment ? complete(JSON.stringify(this.results.judgment,null,2)) : absent('No judgment recorded.'));
    if(this.record)panel.append(node('p',this.record.cleanup?.completed_at_unix_ms?'Runtime cleanup complete.':'Runtime cleanup pending; recovery is available in Ways History.'));
  }
  async inspect(candidate) {
    this.renderEvidence({...candidate,...this.inspections.get(candidate.id.index)}); if(this.record)return;
    const sequence=this.sequence, index=candidate.id.index;
    const params=new URLSearchParams({attempt_set_id:this.setId,baseline:String(index)});
    const results=await Promise.allSettled([this.request(`variants/trajectories?${params}`),this.request(`variants/status?attempt_set_id=${encodeURIComponent(this.setId)}`)]);
    if(sequence!==this.sequence||this.record||this.selected!==index)return;
    const projected={...candidate,liveDiffs:this.inspections.get(index)?.liveDiffs || new Map()};
    if(results[0].status==='fulfilled') {const alignment=results[0].value,position=alignment.lanes?.indexOf(index);projected.route=position>=0?complete(JSON.stringify((alignment.rows||[]).map(row=>row.steps?.[position] ?? row.cells?.[position] ?? null),null,2)):absent('No Route recorded for this candidate.');}
    else projected.route=absent(results[0].reason.message);
    const status=results[1].status==='fulfilled'?results[1].value?.find(item=>item.index===index):null;
    projected.changed_paths={items:status?.status?.files?.map(file=>file.path)||[]};
    projected.reviewable_diff=absent(status?.review_error || (results[1].status==='rejected'?results[1].reason.message:'Choose a changed path below.'));
    this.inspections.set(index,projected); this.renderEvidence(projected);
    for(const file of status?.status?.files||[]) {const button=node('button',`Read diff: ${file.path}`);button.onclick=async()=>{button.disabled=true;const params=new URLSearchParams({attempt_set_id:this.setId,index:String(index),path:file.path});try{const diff=await this.request(`variants/diff?${params}`);if(sequence!==this.sequence||this.record||this.selected!==index)return;const text=typeof diff==='string'?diff:diff.diff??JSON.stringify(diff,null,2);projected.liveDiffs.set(file.path,text);this.q('.evidence').append(node('h3',file.path),node('pre',text));}catch(error){if(sequence===this.sequence&&this.selected===index)this.q('.evidence').append(node('p',error.message));}finally{button.disabled=false;}};this.q('.evidence').append(button);}
  }
}
customElements.define('ax-ways-graph',AxWaysGraph);
