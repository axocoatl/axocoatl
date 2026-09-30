const node=(tag,text,attrs={})=>{const item=document.createElement(tag);if(text!=null)item.textContent=text;for(const[key,value]of Object.entries(attrs))item.setAttribute(key,value);return item;};
const limits=[['activations','Activation limit'],['invocations','Invocation limit'],['tokens','Token limit'],['cost_microunits','Cost limit (USD)']];
// The Team view sends roles as the daemon names them (Worker, Coordinator);
// compare them without case.
const role=item=>String(item.role||'autonomous').toLowerCase();
// Any Agent except a Worker may delegate. A Coordinator template must, and runs as such a lead.
const canDelegate=slot=>role(slot)!=='worker';
// A helper starts unapproved: the person enters its limits.
const workerDraft=template=>({template_id:template.template_id,limits:{activations:null,invocations:null,tokens:null,cost_microunits:null},max_output_tokens:template.max_output_tokens??null,adhoc_allowed:false});
// The daemon proposes read-only helpers for a new Session's single Agent. The
// proposal becomes that Agent's draft approval; nothing is approved until Apply.
export function proposeDelegation(view,draft){
 const proposal=view.proposed_delegation,slot=proposal&&draft.slots.find(item=>item.slot_id===proposal.slot_id);
 if(!slot||slot.delegation||!canDelegate(slot))return null;
 const helpers=proposal.helpers.map(id=>view.templates.find(template=>template.template_id===id)).filter(Boolean);
 if(!helpers.length)return null;
 slot.delegation={workers:helpers.map(workerDraft),operations:[...proposal.operations],max_nodes:proposal.max_nodes,max_edges:proposal.max_edges};
 return{slot,helpers};
}
export function renderDelegationApproval(host,panel,slot,update){
 if(!canDelegate(slot))return;
 panel.append(node('h3','Helpers this Agent may delegate to'),node('p','With helpers approved, this Agent gets a delegate tool. Each helper starts in its own conversation with only the task it is given, and the limits you approve below are reserved from this Agent\'s budget above while the helper runs; what the helper does not use goes back to this Agent when it finishes. A helper must be read-only: its template has writes: [] or no tool that writes files or runs commands; any other helper is refused.'));
 const latest=()=>host.draft.slots.find(item=>item.slot_id===slot.slot_id).delegation;
 const save=change=>{const next=structuredClone(latest());change(next);update('delegation',next);};
 const enabled=host.mode==='edit'&&!host.busy;
 const check=(parent,label,checked,onchange)=>{const wrapper=node('label',label,{class:'check'}),input=node('input',null,{type:'checkbox'});input.checked=checked;input.disabled=!enabled;input.onchange=()=>onchange(input.checked);wrapper.prepend(input);parent.append(wrapper);return input;};
 check(panel,'Let this Agent delegate to helpers',!!slot.delegation,checked=>{update('delegation',checked?{workers:[],operations:['add_agent'],max_nodes:null,max_edges:null}:null);host.renderPanel();});
 if(!slot.delegation)return;
 for(const[key,label,min]of [['max_nodes','Maximum Agents in the turn, helpers included',1],['max_edges','Maximum connections in the turn graph',0]]){const wrapper=node('label',label),input=node('input',null,{type:'number',min,step:1});input.value=slot.delegation[key]??'';input.disabled=!enabled;input.oninput=()=>save(value=>{value[key]=input.value===''?null:Number(input.value);});wrapper.append(input);panel.append(wrapper);}
 panel.append(node('h4','Helper templates'));
 const templates=host.view.templates.filter(template=>role(template)==='worker');
 for(const worker of slot.delegation.workers)if(!templates.some(template=>template.template_id===worker.template_id))templates.push({template_id:worker.template_id,name:worker.template_id,model:'Previously approved definition'});
 if(!templates.length)panel.append(node('p','Add a Worker in Settings → Agents, then refresh this Session team.'));
 for(const template of templates){const selected=slot.delegation.workers.find(worker=>worker.template_id===template.template_id);
  check(panel,`Use helper: ${template.name}`,!!selected,checked=>{save(value=>{value.workers=value.workers.filter(worker=>worker.template_id!==template.template_id);if(checked)value.workers.push(workerDraft(template));});host.renderPanel();});
  if(!selected)continue;
  const group=node('fieldset'),legend=node('legend',`${template.name} · ${template.model}`);group.append(legend);panel.append(group);
  const workerChange=change=>save(value=>change(value.workers.find(worker=>worker.template_id===template.template_id)));
  for(const[key,label]of limits){const wrapper=node('label',`${template.name}: ${label}`),input=node('input',null,{type:'number',min:key==='cost_microunits'?0:1,step:key==='cost_microunits'?0.000001:1});input.value=selected.limits[key]==null?'':key==='cost_microunits'?selected.limits[key]/1e6:selected.limits[key];input.disabled=!enabled;input.oninput=()=>workerChange(worker=>{worker.limits[key]=input.value===''?null:key==='cost_microunits'?Math.round(Number(input.value)*1e6):Number(input.value);});wrapper.append(input);group.append(wrapper);}
  const outputLabel=node('label',`${template.name}: Maximum output tokens per request`),output=node('input',null,{type:'number',min:1,step:1});output.value=selected.max_output_tokens??'';output.disabled=!enabled;output.oninput=()=>workerChange(worker=>{worker.max_output_tokens=output.value===''?null:Number(output.value);});outputLabel.append(output);group.append(outputLabel);
 }
}
export function validateDelegationApproval(slot){
 if(!canDelegate(slot))return;
 const policy=slot.delegation;
 if(!policy&&role(slot)!=='coordinator')return;
 if(!policy?.workers?.length||!policy.operations?.includes('add_agent')||!Number.isSafeInteger(policy.max_nodes)||policy.max_nodes<1||!Number.isSafeInteger(policy.max_edges)||policy.max_edges<0)throw new Error(`Select at least one helper and explicit graph bounds for ${slot.name}.`);
 for(const worker of policy.workers){if(!Number.isSafeInteger(worker.max_output_tokens)||worker.max_output_tokens<1||!limits.every(([key])=>Number.isSafeInteger(worker.limits[key])&&worker.limits[key]>=(key==='cost_microunits'?0:1)&&worker.limits[key]<=slot.limits[key]))throw new Error(`Enter explicit limits within ${slot.name}'s budget and a maximum output for helper ${worker.template_id}.`);}
}
