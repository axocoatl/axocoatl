// The live signal field for a Session's armed signal sources: what each Agent
// senses on the paths it owns, every deposit and why it counts or not, and the
// cause chain from deposit to dispatched turn to the deposits that turn left.
// Signal strength is advisory; the inbox, grants and checks remain authority.
import {adopt} from './sheets.js';
const h=(tag,text,attrs={})=>{const element=document.createElement(tag);if(text!=null)element.textContent=text;for(const[key,value]of Object.entries(attrs))element.setAttribute(key,value);return element;};
const button=(text,onclick,attrs={})=>{const element=h('button',text,{type:'button',...attrs});element.onclick=onclick;return element;};
const ownsText=(sensor)=>!Array.isArray(sensor.owns)||sensor.owns.join('\n')===sensor.watches.join('\n')?'':sensor.owns.length?` · may change ${sensor.owns.join(', ')}`:' · read-only';
const fixed=value=>Number(value||0).toFixed(2);
const ago=(then,now)=>{const seconds=Math.max(0,Math.round((now-then)/1000));if(seconds<60)return`${seconds}s ago`;const minutes=Math.round(seconds/60);if(minutes<60)return`${minutes} min ago`;return`${Math.round(minutes/60)} h ago`;};
const KIND={finding:'Finding',pitfall:'Pitfall',change:'Change',check_failure:'Failed check',human:'Flag'};
const EXCLUDED={own_deposit:'left by this Agent',consumed:'already acted on',source_changed:'source changed since',source_missing:'source no longer present',withdrawn:'withdrawn',out_of_scope:'outside its paths'};
const STATE={quiet:'Quiet',sensing:'Sensing',crossed:'Threshold crossed',latched:'Crossed; dispatches after earlier work',waiting:'Work waiting',capped:'Episode dispatch limit reached',budget:'Episode token budget spent',repeat:'Held: same evidence as before'};
const RECEIPT={queued:'pending',reserved:'pending',running:'running',settled:'success',completed:'success',finished:'success',dismissed:'cancelled',blocked:'blocked',needsattention:'error',cancelled:'cancelled'};
const MIN_READABLE_SCALE=0.6;
const turnOf=cause=>cause?.turn_id||null;
const stateText=state=>state==='needsattention'?'needs attention':state;
const STYLE=`:host{display:block;margin:12px 0}*{box-sizing:border-box}section.field{border:1px solid var(--border);border-radius:8px;padding:10px;margin:8px 0}header{display:flex;flex-wrap:wrap;gap:8px;align-items:center}header h4{margin:0;flex:1;min-width:12rem;font-size:var(--fs-body)}.muted{color:var(--muted)}.error{color:var(--err)}.sensors{display:grid;grid-template-columns:repeat(auto-fill,minmax(200px,1fr));gap:8px;margin:10px 0}.sensor{border:1px solid var(--border);border-radius:8px;padding:8px;display:flex;flex-direction:column;gap:4px}.sensor>.badge,.sensor>button{align-self:flex-start}.sensor[data-state="crossed"],.sensor[data-state="latched"],.sensor[data-state="waiting"]{border-color:var(--accent)}.sensor[data-state="capped"]{border-color:var(--warn)}.bar{position:relative;height:8px;border-radius:4px;background:var(--bg-3);overflow:visible}.bar>span{position:absolute;inset:0 auto 0 0;border-radius:4px;background:var(--accent);transition:width var(--dur-base,.2s) var(--ease,ease)}.bar>i{position:absolute;top:-3px;bottom:-3px;width:2px;background:var(--text)}.badge{display:inline-block;font-size:var(--fs-xs);padding:1px 6px;border-radius:var(--r-pill,99px);border:1px solid var(--border);white-space:nowrap}.kind-finding,.kind-pitfall{border-color:var(--warn)}.kind-check_failure{border-color:var(--err)}.kind-human{border-color:var(--accent)}.row{padding:8px 0;border-top:1px solid var(--border);overflow-wrap:anywhere}.row.off{opacity:.62}.row.root{border-left:3px solid var(--border);padding-left:8px}.row.held{border-left:3px solid var(--warn);padding-left:8px}.chain-left{margin-left:12px}.row p{margin:3px 0}.actions{display:flex;flex-wrap:wrap;gap:6px;margin-top:4px}button,input,textarea{font:inherit;color:inherit;background:var(--bg-2);border:1px solid var(--border);border-radius:6px;padding:5px 8px}button{cursor:pointer}button:disabled{opacity:.5;cursor:default}textarea{width:100%;min-height:52px}label{display:flex;flex-direction:column;gap:4px;margin:6px 0}.graph-wrap{height:320px;border:1px solid var(--border);border-radius:8px;overflow:hidden;position:relative}.graph-wrap.expanded{height:min(75vh,720px)}ax-lattice{width:100%;height:100%}ax-node{width:190px;height:64px;overflow:hidden}ax-node strong,ax-node small{display:block;font-size:var(--fs-xs);line-height:1.3;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:170px}ax-node small{color:var(--muted)}details>summary{cursor:pointer;margin:6px 0}code{font-family:var(--font-mono);font-size:var(--fs-xs)}@media(prefers-reduced-motion:reduce){.bar>span{transition:none}}@media(max-width:520px){.sensors{grid-template-columns:1fr}.graph-wrap{height:260px}}`;

export class AxSessionSignals extends HTMLElement {
  static observedAttributes=['session'];
  constructor(){super();this.attachShadow({mode:'open'});this.root=h('div');this.shadowRoot.append(this.root);void adopt(this.shadowRoot,STYLE,['/ui/tokens.css']);this.views=[];this.receipts=[];this.sequence=0;this.busy=false;this.ui=new Map();}
  uiFor(id){if(!this.ui.has(id))this.ui.set(id,{graphOpen:true,graphExpanded:false,depositsOpen:true});return this.ui.get(id);}
  get session(){return this.getAttribute('session')||'';}
  attributeChangedCallback(name,oldValue,value){if(oldValue!==value){this.sequence++;this.views=[];this.receipts=[];this.root.replaceChildren();}}
  labelFor(node){for(const view of this.views)for(const sensor of view.sensors)if(sensor.node_id===node)return sensor.label;return null;}
  async request(suffix,body){const response=await fetch(`/api/sessions/${encodeURIComponent(this.session)}/work${suffix}`,{method:body===undefined?'GET':'POST',...(body===undefined?{}:{headers:{'content-type':'application/json'},body:JSON.stringify(body)})});const result=await response.json();if(!response.ok)throw new Error(result.error||`Signal request failed (${response.status})`);return result;}
  async load(){if(!this.session||this.busy)return;const sequence=this.sequence;try{const [views,work]=await Promise.all([this.request('/signals'),this.request('')]);if(sequence!==this.sequence)return;this.views=Array.isArray(views)?views:[];this.receipts=work.receipts||[];this.error=null;}catch(error){if(sequence!==this.sequence)return;this.error=error.message;}this.render();}
  async act(suffix,body,message,clear=[]){if(this.busy)return;const sequence=this.sequence;this.busy=true;this.render();try{const views=await this.request(suffix,body);if(sequence!==this.sequence)return;if(!Array.isArray(views))throw new Error('The signal field response was not recognized.');for(const key of clear){const field=this.root.querySelector(`[data-key="${CSS.escape(key)}"]`);if(field)field.value='';}this.views=views;this.notice=message;this.error=null;const work=await this.request('');if(sequence===this.sequence)this.receipts=work.receipts||[];}catch(error){if(sequence===this.sequence)this.error=error.message;}finally{if(sequence===this.sequence){this.busy=false;this.render();this.dispatchEvent(new CustomEvent('signals-changed',{bubbles:true,composed:true}));}}}
  openTurn(turn){this.dispatchEvent(new CustomEvent('open-session-work-turn',{bubbles:true,composed:true,detail:{session_id:this.session,turn_id:turn}}));}
  receiptState(id){const item=this.receipts.find(entry=>entry.receipt.receipt_id===id);return item?item.state:null;}
  render(){
    // A refresh must not discard what a person is typing or where focus was.
    const focused=this.shadowRoot.activeElement?.dataset?.key;
    const drafts=new Map([...this.root.querySelectorAll('input[data-key],textarea[data-key]')].filter(field=>field.value).map(field=>[field.dataset.key,field.value]));
    this.root.replaceChildren();
    if(!this.views.length){if(this.error)this.root.append(h('p',this.error,{class:'error',role:'status'}));return;}
    for(const view of this.views)this.root.append(this.renderField(view));
    if(this.error)this.root.append(h('p',this.error,{class:'error',role:'status'}));else if(this.notice)this.root.append(h('p',this.notice,{class:'muted',role:'status'}));
    for(const field of this.root.querySelectorAll('input[data-key],textarea[data-key]'))if(drafts.has(field.dataset.key))field.value=drafts.get(field.dataset.key);
    if(focused)this.shadowRoot.querySelector(`[data-key="${CSS.escape(focused)}"]`)?.focus({preventScroll:true});
  }
  renderField(view){
    const section=h('section',null,{class:'field','aria-label':`Signal field ${view.event_kind}`});
    const header=h('header');
    header.append(h('h4',`Signal field · ${view.armed?'armed':'disarmed'}`),h('span',`episode ${view.episode??1} · ${view.automatic_dispatches}/${view.max_dispatches} automatic dispatches`,{class:'badge'}),h('span',view.half_life_ms?`half-life ${Math.round(view.half_life_ms/60000)} min`:'no evaporation',{class:'badge'}));
    const status=view.episode_status==='quiet'?`Quiet since ${ago(view.quiet_since_ms,view.now_ms)}${view.required_checks?' on visible checks':' (no required checks are configured)'}. The signals stopped; that does not show the work is correct. A new deposit starts the next episode.`:view.episode_status==='held'?'Held: every crossing is waiting for a person (limit, budget or repeated evidence). Use Send now on an Agent to continue.':'Active: signal work is crossing thresholds or running.';
    const tokens=view.max_episode_tokens?` Tokens this episode: ${view.episode_tokens} of ${view.max_episode_tokens}.`:view.episode_tokens?` Tokens this episode: ${view.episode_tokens}.`:'';
    const sense=button('Sense now',()=>void this.act(`/signals/${encodeURIComponent(view.binding_id)}/sense`,{},'Sources observed; any crossed threshold was dispatched.'),{'data-key':`sense:${view.binding_id}`});sense.disabled=this.busy||!view.armed;header.append(sense);
    section.append(header,h('p',`${status}${tokens}`,{class:'muted episode-status','data-episode-status':view.episode_status||'active'}));
    section.append(h('p',view.observed_at_ms?`Sources last observed ${ago(view.observed_at_ms,view.now_ms)}${view.observation_truncated?' (watched file limit reached; some paths were not observed)':''}. A deposit counts for an Agent only on paths it watches, until that Agent acts on it or its cited source changes.`:'Sources are observed once the Session runtime is running.',{class:'muted'}));
    if(view.error){section.append(h('p',view.error,{class:'error'}));return section;}
    const sensors=h('div',null,{class:'sensors'});
    for(const sensor of view.sensors){
      const card=h('div',null,{class:'sensor','data-state':sensor.state});
      const scale=Math.max(sensor.threshold*1.5,sensor.intensity,0.001);
      const bar=h('div',null,{class:'bar',role:'meter','aria-valuemin':'0','aria-valuemax':fixed(scale),'aria-valuenow':fixed(sensor.intensity),'aria-label':`${sensor.label} signal ${fixed(sensor.intensity)} of threshold ${fixed(sensor.threshold)}`});
      const fill=h('span');fill.style.width=`${Math.min(100,sensor.intensity/scale*100)}%`;const tick=h('i');tick.style.left=`${Math.min(100,sensor.threshold/scale*100)}%`;bar.append(fill,tick);
      card.append(h('strong',sensor.label),h('span',STATE[sensor.state]||sensor.state,{class:'badge'}),h('small',`Watches ${sensor.watches.join(', ')}${ownsText(sensor)}`,{class:'muted'}),bar,h('small',`${fixed(sensor.intensity)} / ${fixed(sensor.threshold)}`));if(sensor.held_reason)card.append(h('small',sensor.held_reason,{class:'muted held-reason'}));
      const live=sensor.deposits.filter(item=>!item.excluded).length;
      if(live&&sensor.state!=='waiting'){const send=button(`Send to ${sensor.label} now`,()=>void this.act(`/signals/${encodeURIComponent(view.binding_id)}/routes/${encodeURIComponent(sensor.slot_id)}/dispatch`,{},`Dispatched ${sensor.label} by hand.`),{'data-key':`send:${sensor.slot_id}`});send.disabled=this.busy||!view.armed;card.append(send);}
      sensors.append(card);
    }
    section.append(sensors);
    const ui=this.uiFor(view.binding_id);
    // Setting open below queues a toggle too; only a person's toggle acts.
    const graph=h('details');graph.open=ui.graphOpen;graph.ontoggle=()=>{if(graph.open===ui.graphOpen)return;ui.graphOpen=graph.open;if(graph.open)void this.renderGraph(host,view);};graph.append(h('summary','Cause graph: deposit → dispatch → turn → new deposits',{'data-key':`graph:${view.binding_id}`}));
    const expand=button(ui.graphExpanded?'Shrink graph':'Expand graph',()=>{ui.graphExpanded=!ui.graphExpanded;expand.textContent=ui.graphExpanded?'Shrink graph':'Expand graph';expand.setAttribute('aria-pressed',String(ui.graphExpanded));host.querySelector('.graph-wrap')?.classList.toggle('expanded',ui.graphExpanded);},{'data-key':`expand:${view.binding_id}`,'aria-pressed':String(ui.graphExpanded)});
    const host=h('div'),graphActions=h('div',null,{class:'actions'});graphActions.append(expand);graph.append(graphActions,host);section.append(graph);if(graph.open)void this.renderGraph(host,view);
    section.append(this.renderChain(view));
    const deposits=h('details',null,{class:'deposits'});deposits.open=ui.depositsOpen;deposits.ontoggle=()=>{ui.depositsOpen=deposits.open;};deposits.append(h('summary',`Deposits (${view.deposits.length}): strength, who senses them, withdraw`,{'data-key':`deposits:${view.binding_id}`}));
    if(!view.deposits.length)deposits.append(h('p','No deposits yet. Findings, source changes, failed checks and flags appear here.',{class:'muted'}));
    for(const deposit of [...view.deposits].reverse())deposits.append(this.renderDeposit(view,deposit));
    section.append(deposits);
    if(view.stranded?.length)section.append(this.renderStranded(view));
    section.append(this.renderFlag(view));
    return section;
  }
  depositText(view,deposit){const live=view.sensors.map(sensor=>({sensor,item:sensor.deposits.find(item=>item.deposit===deposit.id&&!item.excluded)})).filter(entry=>entry.item);const sensed=live.map(({sensor,item})=>item.counted_with?`${sensor.label} (counted once with its source)`:sensor.label);return`${KIND[deposit.kind]||deposit.kind} on ${deposit.paths.join(', ')}${deposit.withdrawn?' (withdrawn)':sensed.length?` → sensed by ${sensed.join(', ')}`:deposit.routed?' → already acted on or superseded':' → nobody watches these paths'}`;}
  renderChain(view){
    // Read top to bottom: what each dispatch was sent for, how it ended, and
    // which new signals its turn left for the next Agent.
    const box=h('div',null,{class:'chain'});
    box.append(h('h4',`Cause chain (${view.dispatches.length} dispatch${view.dispatches.length===1?'':'es'})`));
    const byTurn=new Map();for(const deposit of view.deposits){const turn=turnOf(deposit.cause);if(turn)(byTurn.get(turn)||byTurn.set(turn,[]).get(turn)).push(deposit);}
    const dispatchedTurns=new Set(view.dispatches.map(item=>item.turn_id).filter(Boolean));
    const roots=view.deposits.filter(deposit=>!dispatchedTurns.has(turnOf(deposit.cause)));
    const entries=[...roots.map(deposit=>({at:deposit.deposited_at_ms,deposit})),...view.dispatches.map(dispatch=>({at:dispatch.at_ms,dispatch}))].sort((a,b)=>a.at-b.at);
    if(!entries.length)box.append(h('p','No deposits or dispatches yet. Each dispatch appears here with the signals that caused it and the signals its turn left.',{class:'muted'}));
    let step=0;
    for(const entry of entries){
      const row=h('div',null,{class:'row'});
      if(entry.deposit){const deposit=entry.deposit;row.classList.add('root');if(deposit.withdrawn)row.classList.add('off');row.append(h('p',`Signal · ${ago(deposit.deposited_at_ms,view.now_ms)} · ${deposit.producer_label?`left by ${deposit.producer_label}`:deposit.cause.kind==='human'?'flagged by a person':deposit.cause.kind==='turn'?'observed after a Session turn':'recorded by the host'}`,{class:'muted'}),h('p',this.depositText(view,deposit)));box.append(row);continue;}
      const dispatch=entry.dispatch,item=dispatch.receipt_id?this.receipts.find(value=>value.receipt.receipt_id===dispatch.receipt_id):null,state=item?.state||null;step++;
      row.dataset.state=state||dispatch.disposition||'pending';
      if(['dismissed','cancelled'].includes(state))row.classList.add('off');
      row.append(h('p',`${step}. ${dispatch.label} · ${fixed(dispatch.intensity)} ${dispatch.manual?(dispatch.intensity<dispatch.threshold?'sent by a person below':'sent by a person at or above'):dispatch.latched?'held from an earlier crossing of':'≥'} threshold ${fixed(dispatch.threshold)} · ${ago(dispatch.at_ms,view.now_ms)}`));
      row.append(h('p',`Because: ${dispatch.contributions.map(contribution=>{const deposit=view.deposits.find(value=>value.id===contribution.deposit);return deposit?`${KIND[deposit.kind]||deposit.kind} on ${deposit.paths.join(', ')}${deposit.producer_label?` by ${deposit.producer_label}`:''} (${contribution.counted_with?'same source':fixed(contribution.weight)})`:contribution.deposit;}).join('; ')||'sent without live signals'}`,{class:'muted'}));
      const reason=item?.reason&&['dismissed','blocked','needsattention','cancelled'].includes(state)?`: ${item.reason}`:'';
      row.append(h('p',`Work ${stateText(state||dispatch.disposition||'not admitted')}${reason}`,{class:'muted chain-result'}));
      const left=dispatch.turn_id?byTurn.get(dispatch.turn_id)||[]:[];
      if(left.length)for(const deposit of left)row.append(h('p',`↳ left ${this.depositText(view,deposit)}`,{class:'chain-left'}));
      else if(['settled','completed','finished'].includes(state))row.append(h('p','↳ left no new signal',{class:'muted chain-left'}));
      if(dispatch.turn_id&&state&&!['queued','dismissed'].includes(state)){const actions=h('div',null,{class:'actions'});actions.append(button('Inspect dispatched turn',()=>this.openTurn(dispatch.turn_id),{'data-key':`turn:${dispatch.id}`}));row.append(actions);}
      box.append(row);
    }
    for(const sensor of view.sensors.filter(value=>value.held_reason)){const row=h('div',null,{class:'row held'});row.append(h('p',`Now · ${sensor.label} is held: ${sensor.held_reason}`));box.append(row);}
    return box;
  }
  renderDeposit(view,deposit){
    const excluded=view.sensors.flatMap(sensor=>sensor.deposits.filter(item=>item.deposit===deposit.id).map(item=>({sensor,item})));
    const live=excluded.filter(entry=>!entry.item.excluded);
    const voting=live.filter(entry=>!entry.item.counted_with),riding=live.filter(entry=>entry.item.counted_with);
    const summaryOf=id=>view.deposits.find(item=>item.id===id)?.summary||id;
    const row=h('div',null,{class:`row${deposit.withdrawn||(!live.length&&deposit.routed)?' off':''}`});
    const title=h('p');title.append(h('span',KIND[deposit.kind]||deposit.kind,{class:`badge kind-${deposit.kind}`}),document.createTextNode(` ${deposit.paths.join(', ')}`));
    row.append(title,h('p',deposit.summary));
    const weight=voting[0]?.item.weight??(riding.length?null:excluded[0]?.item.weight);
    row.append(h('p',`${deposit.producer_label?`Left by ${deposit.producer_label}`:deposit.cause.kind==='turn'?'Observed after a Session turn':deposit.cause.kind==='human'?'Left by a person':'Recorded by the host'} · ${ago(deposit.deposited_at_ms,view.now_ms)} · strength ${fixed(deposit.strength)}${weight!=null?` → ${fixed(weight)} now`:''}`,{class:'muted'}));
    if(deposit.withdrawn)row.append(h('p',`Withdrawn: ${deposit.withdrawn}`,{class:'muted'}));
    else if(!deposit.routed)row.append(h('p','No Agent watches these paths; nobody is woken by it.',{class:'muted'}));
    else row.append(h('p',[voting.length?`Sensed by ${voting.map(entry=>entry.sensor.label).join(', ')}`:null,...riding.map(entry=>`${entry.sensor.label}: counted once with "${summaryOf(entry.item.counted_with)}" (same source)`),...excluded.filter(entry=>entry.item.excluded).map(entry=>`${entry.sensor.label}: ${EXCLUDED[entry.item.excluded]||entry.item.excluded}`)].filter(Boolean).join(' · '),{class:'muted'}));
    const changedElsewhere=[...new Set(live.flatMap(entry=>entry.item.evidence_changed||[]))];
    if(changedElsewhere.length)row.append(h('p',`${changedElsewhere.join(', ')} changed since this was recorded; whoever acts on it verifies what still applies.`,{class:'muted'}));
    const actions=h('div',null,{class:'actions'});const turn=turnOf(deposit.cause);
    if(turn)actions.append(button('Inspect source turn',()=>this.openTurn(turn),{'data-key':`source:${deposit.id}`}));
    if(!deposit.withdrawn&&view.armed){const reason=h('input',null,{'aria-label':`Reason for withdrawing ${deposit.summary}`,placeholder:'Reason to withdraw','data-key':`reason:${deposit.id}`});const withdraw=button('Withdraw',()=>{if(!reason.value.trim()){this.error='Enter a reason for withdrawing this signal.';this.render();return;}void this.act(`/signals/${encodeURIComponent(view.binding_id)}/deposits/${encodeURIComponent(deposit.id)}/withdraw`,{reason:reason.value},'Signal withdrawn.',[`reason:${deposit.id}`]);},{'data-key':`withdraw:${deposit.id}`});withdraw.disabled=this.busy;actions.append(reason,withdraw);}
    if(actions.childElementCount)row.append(actions);
    return row;
  }
  renderStranded(view){
    const box=h('div',null,{class:'stranded'});
    box.append(h('h4','Findings that were not published'),h('p','These came from turns that failed, were stopped or wait for attention, or from work that was not accepted when its turn closed, so they leave no signal. Accepting one publishes it to workspace knowledge as accepted by you and lets it reach the Agent that watches its files. Dismissing rejects it.',{class:'muted'}));
    for(const item of view.stranded){
      const row=h('div',null,{class:'row'});
      row.append(h('p',item.title),h('p',`${item.paths.join(', ')} · turn ${item.turn_id.slice(-8)} · ${item.turn_state.replaceAll('_',' ')}`,{class:'muted'}));
      const actions=h('div',null,{class:'actions'});
      const accept=button('Accept as signal',()=>void this.decide(view,item,true),{'data-key':`accept:${item.proposal_id}`});
      const dismiss=button('Dismiss finding',()=>void this.decide(view,item,false),{'data-key':`dismiss:${item.proposal_id}`});
      accept.disabled=dismiss.disabled=this.busy||!view.armed;
      actions.append(accept,dismiss,button('Inspect source turn',()=>this.openTurn(item.turn_id),{'data-key':`stranded-turn:${item.proposal_id}`}));
      row.append(actions);box.append(row);
    }
    return box;
  }
  async decide(view,item,accept){
    if(this.busy)return;this.busy=true;this.render();
    try{
      const response=await fetch(`/api/sessions/${encodeURIComponent(this.session)}/knowledge/proposals/${encodeURIComponent(item.proposal_id)}/${accept?'accept':'reject'}`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(accept?{expected_revision:item.expected_revision}:{})});
      if(!response.ok){const result=await response.json().catch(()=>({}));throw new Error(result.error||`Knowledge decision failed (${response.status})`);}
    }catch(error){this.busy=false;this.error=error.message||String(error);this.render();return;}
    this.busy=false;
    await this.act(`/signals/${encodeURIComponent(view.binding_id)}/sense`,{},accept?'Finding accepted and published.':'Finding dismissed.');
    // Say what the field did with it rather than assume it signals someone.
    if(accept&&!this.error){const field=this.views.find(entry=>entry.binding_id===view.binding_id),deposit=field?.deposits.find(entry=>entry.id===`finding:${item.proposal_id}`);this.notice=!deposit?'Finding accepted and published; the field did not record a signal for it.':deposit.routed?`Finding accepted; it now signals ${deposit.sensed_by.length?field.sensors.filter(sensor=>deposit.sensed_by.includes(sensor.slot_id)||deposit.sensed_by.includes(sensor.node_id)).map(sensor=>sensor.label).join(', ')||'the Agent that watches its files':'no one yet (its watchers already acted or it is their own)'}.`:'Finding accepted; no Agent watches its files, so it wakes no one.';this.render();}
  }
  renderFlag(view){
    const form=h('form');form.onsubmit=event=>event.preventDefault();
    form.append(h('h4','Flag paths'),h('p','A flag is sensed like a finding by whoever watches those paths. It stops counting once they act on it or the flagged file changes.',{class:'muted'}));
    const paths=h('input',null,{'aria-label':'Paths to flag','placeholder':'lib/manifest.js, lib/paths.js','data-key':`flag-paths:${view.binding_id}`});const summary=h('textarea',null,{'aria-label':'What should be looked at','placeholder':'What should be looked at','data-key':`flag-summary:${view.binding_id}`});
    const submit=button('Flag paths',()=>{const list=paths.value.split(/[\n,]/).map(value=>value.trim()).filter(Boolean);if(!list.length||!summary.value.trim()){this.error='Enter at least one repository path and what should be looked at.';this.render();return;}void this.act(`/signals/${encodeURIComponent(view.binding_id)}/flags`,{paths:list,summary:summary.value},'Flag recorded.',[`flag-paths:${view.binding_id}`,`flag-summary:${view.binding_id}`]);},{'data-key':`flag:${view.binding_id}`});
    submit.disabled=this.busy||!view.armed;
    const pathLabel=h('label','Repository paths');pathLabel.append(paths);const summaryLabel=h('label','What should be looked at');summaryLabel.append(summary);
    form.append(pathLabel,summaryLabel,submit);return form;
  }
  async renderGraph(host,view){
    const nodes=[],edges=[],ids=new Map();
    const add=(key,title,detail,status,open)=>{if(ids.has(key))return ids.get(key);const id=`signal-node-${nodes.length}`;ids.set(key,id);nodes.push({id,title,detail,status,open});return id;};
    const dispatchByTurn=new Map(view.dispatches.filter(item=>item.turn_id).map(item=>[item.turn_id,item]));
    for(const dispatch of view.dispatches){const state=dispatch.receipt_id?this.receiptState(dispatch.receipt_id):null;add(`dispatch:${dispatch.id}`,`${dispatch.label}`,`${dispatch.manual?'sent by a person':'dispatched'} · ${stateText(state||dispatch.disposition||'pending')}`,RECEIPT[state]||'pending',dispatch.turn_id?()=>this.openTurn(dispatch.turn_id):null);}
    for(const deposit of view.deposits){
      const turn=turnOf(deposit.cause),origin=turn&&dispatchByTurn.get(turn);
      const id=add(`deposit:${deposit.id}`,`${KIND[deposit.kind]||deposit.kind}: ${deposit.paths[0]}${deposit.paths.length>1?` +${deposit.paths.length-1}`:''}`,deposit.summary.slice(0,70),deposit.withdrawn?'cancelled':'idle',turn?()=>this.openTurn(turn):null);
      const label=deposit.kind==='change'?'changed':deposit.kind==='check_failure'?'failed check':'recorded';
      if(origin)edges.push({from:ids.get(`dispatch:${origin.id}`),to:id,label});
      else if(turn){const source=add(`turn:${turn}`,'Session turn',turn.slice(-8),'success',()=>this.openTurn(turn));edges.push({from:source,to:id,label});}
    }
    for(const dispatch of view.dispatches)for(const contribution of dispatch.contributions){const from=ids.get(`deposit:${contribution.deposit}`);if(from)edges.push({from,to:ids.get(`dispatch:${dispatch.id}`),label:contribution.counted_with?'same source':fixed(contribution.weight),active:''});}
    if(!nodes.length){host.replaceChildren(h('p','The graph appears after the first deposit.',{class:'muted'}));return;}
    // Reuse the drawn graph while its content is unchanged so polling does not
    // reset the person's pan and zoom.
    const signature=JSON.stringify([nodes.map(({id,title,detail,status})=>[id,title,detail,status]),edges]);
    const cached=this.graphs?.get(view.binding_id);
    if(cached?.signature===signature){cached.wrapper.classList.toggle('expanded',this.uiFor(view.binding_id).graphExpanded);host.replaceChildren(cached.wrapper);return;}
    host.replaceChildren(h('p','Loading cause graph…',{class:'muted'}));
    let layeredLayout;
    try{await import('../lattice/index.js');({layeredLayout}=await import('../lattice/layout.js'));await customElements.whenDefined('ax-node');}catch(error){host.replaceChildren(h('p',`Cause graph unavailable: ${error.message}`,{class:'error'}));return;}
    if(!host.isConnected)return;
    host.replaceChildren();
    const wrapper=h('div',null,{class:`graph-wrap${this.uiFor(view.binding_id).graphExpanded?' expanded':''}`}),graph=h('ax-lattice',null,{mode:'view','aria-label':'Signal cause graph','data-key':`lattice:${view.binding_id}`});wrapper.append(graph);host.append(wrapper);
    (this.graphs??=new Map()).set(view.binding_id,{signature,wrapper});
    const positions=layeredLayout(nodes.map(node=>({id:node.id,width:190,height:64})),edges,{direction:'LR',gapMain:90,gapCross:24});
    for(const node of nodes){const position=positions.get(node.id)||{x:0,y:0};const element=h('ax-node',null,{id:node.id,'data-x':String(position.x),'data-y':String(position.y),'data-w':'190','data-h':'64',draggable:'false',status:node.status,'aria-label':`${node.title} · ${node.detail}`});element.append(h('strong',node.title),h('small',node.detail));if(node.open)element.addEventListener('node-click',node.open);graph.append(element);}
    graph.addEventListener('node-inspect',event=>nodes.find(node=>node.id===event.detail?.id)?.open?.());
    for(const edge of edges){const attrs={from:edge.from,to:edge.to,label:edge.label};graph.append(h('ax-edge',null,attrs));}
    // Fit once the dialog has laid the canvas out, and again if it resizes.
    // Fitting a long cascade into the frame makes every label unreadable; below
    // a readable scale, show the newest steps (right edge) and let people pan.
    const right=Math.max(...[...positions.values()].map(position=>position.x+190)),top=Math.min(...[...positions.values()].map(position=>position.y));
    let fitted='';const fit=(force=false)=>{const size=`${graph.clientWidth}x${graph.clientHeight}`;if(graph.isConnected&&graph.clientWidth&&(force||size!==fitted)){fitted=size;graph.fitView({padding:24});if(graph.getViewport().k<MIN_READABLE_SCALE)graph.setViewport({k:MIN_READABLE_SCALE,x:graph.clientWidth-24-right*MIN_READABLE_SCALE,y:24-top*MIN_READABLE_SCALE});}};
    new ResizeObserver(()=>fit()).observe(wrapper);requestAnimationFrame(()=>requestAnimationFrame(()=>fit(true)));
  }
}
customElements.define('ax-session-signals',AxSessionSignals);
