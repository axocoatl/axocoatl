const node=(tag,text,attrs={})=>{const item=document.createElement(tag);if(text!=null)item.textContent=text;for(const[key,value]of Object.entries(attrs))item.setAttribute(key,value);return item;};
const operations=[['inspect','Inspect owned work'],['attach_evidence','Attach evidence'],['steer_activation','Guide owned work'],['revise_activation','Revise owned work'],['stop_activation','Stop owned work'],['retry_activation','Retry owned work'],['continue_turn','Continue owned work'],['add_agent','Add work from approved templates'],['replace_future_agent','Replace future owned work'],['finish_normally','Request normal Finish']];
const limits=[['activations','Activation limit'],['invocations','Invocation limit'],['tokens','Token limit'],['cost_microunits','Cost limit (USD)']];
export function renderCoordinatorApproval(host,panel,slot,update){
 if(slot.role!=='coordinator')return;
 panel.append(node('h3','Coordinator authority'),node('p','The Coordinator and all children share its aggregate budget above. Each child also has the limits you approve below. Templates may be reused for distinct conversations within these bounds.'));
 const latest=()=>host.draft.slots.find(item=>item.slot_id===slot.slot_id).delegation;
 const save=change=>{const next=structuredClone(latest());change(next);update('delegation',next);};
 const enabled=host.mode==='edit'&&!host.busy;
 const check=(parent,label,checked,onchange)=>{const wrapper=node('label',label,{class:'check'}),input=node('input',null,{type:'checkbox'});input.checked=checked;input.disabled=!enabled;input.onchange=()=>onchange(input.checked);wrapper.prepend(input);parent.append(wrapper);return input;};
 check(panel,'Approve bounded Coordinator delegation',!!slot.delegation,checked=>{update('delegation',checked?{workers:[],operations:[],max_nodes:null,max_edges:null}:null);host.renderPanel();});
 if(!slot.delegation)return;
 for(const[key,label,min]of [['max_nodes','Maximum nodes in delegated graph',1],['max_edges','Maximum connections in delegated graph',0]]){const wrapper=node('label',label),input=node('input',null,{type:'number',min,step:1});input.value=slot.delegation[key]??'';input.disabled=!enabled;input.oninput=()=>save(value=>{value[key]=input.value===''?null:Number(input.value);});wrapper.append(input);panel.append(wrapper);}
 panel.append(node('h4','Allowed operations'));
 for(const[key,label]of operations)check(panel,label,slot.delegation.operations.includes(key),checked=>save(value=>{value.operations=value.operations.filter(entry=>entry!==key);if(checked)value.operations.push(key);}));
 panel.append(node('h4','Approved Worker templates'));
 const templates=host.view.templates.filter(template=>template.role==='worker');
 for(const worker of slot.delegation.workers)if(!templates.some(template=>template.template_id===worker.template_id))templates.push({template_id:worker.template_id,name:worker.template_id,model:'Previously approved definition'});
 if(!templates.length)panel.append(node('p','Add a Worker in Settings → Agents, then refresh this Session team.'));
 for(const template of templates){const selected=slot.delegation.workers.find(worker=>worker.template_id===template.template_id);
  check(panel,`Use Worker: ${template.name}`,!!selected,checked=>{save(value=>{value.workers=value.workers.filter(worker=>worker.template_id!==template.template_id);if(checked)value.workers.push({template_id:template.template_id,limits:{activations:null,invocations:null,tokens:null,cost_microunits:null},max_output_tokens:template.max_output_tokens??null,adhoc_allowed:false});});host.renderPanel();});
  if(!selected)continue;
  const group=node('fieldset'),legend=node('legend',`${template.name} · ${template.model}`);group.append(legend);panel.append(group);
  const workerChange=change=>save(value=>change(value.workers.find(worker=>worker.template_id===template.template_id)));
  for(const[key,label]of limits){const wrapper=node('label',`${template.name}: ${label}`),input=node('input',null,{type:'number',min:key==='cost_microunits'?0:1,step:key==='cost_microunits'?0.000001:1});input.value=selected.limits[key]==null?'':key==='cost_microunits'?selected.limits[key]/1e6:selected.limits[key];input.disabled=!enabled;input.oninput=()=>workerChange(worker=>{worker.limits[key]=input.value===''?null:key==='cost_microunits'?Math.round(Number(input.value)*1e6):Number(input.value);});wrapper.append(input);group.append(wrapper);}
  const outputLabel=node('label',`${template.name}: Maximum output tokens per request`),output=node('input',null,{type:'number',min:1,step:1});output.value=selected.max_output_tokens??'';output.disabled=!enabled;output.oninput=()=>workerChange(worker=>{worker.max_output_tokens=output.value===''?null:Number(output.value);});outputLabel.append(output);group.append(outputLabel);
  check(group,`${template.name}: Allow selection for ad hoc work`,selected.adhoc_allowed,checked=>workerChange(worker=>{worker.adhoc_allowed=checked;}));
 }
}
export function validateCoordinatorApproval(slot){
 if(slot.role!=='coordinator')return;
 const policy=slot.delegation;
 if(!policy?.workers?.length||!policy.operations.includes('add_agent')||!Number.isSafeInteger(policy.max_nodes)||policy.max_nodes<1||!Number.isSafeInteger(policy.max_edges)||policy.max_edges<0)throw new Error(`Approve Worker templates, Add work permission and explicit graph bounds for ${slot.name}.`);
 for(const worker of policy.workers){if(!Number.isSafeInteger(worker.max_output_tokens)||worker.max_output_tokens<1||!limits.every(([key])=>Number.isSafeInteger(worker.limits[key])&&worker.limits[key]>=(key==='cost_microunits'?0:1)&&worker.limits[key]<=slot.limits[key]))throw new Error(`Enter explicit limits within the Coordinator aggregate and a maximum output for Worker ${worker.template_id}.`);}
}
