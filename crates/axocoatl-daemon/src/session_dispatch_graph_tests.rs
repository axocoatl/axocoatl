use super::*;
use crate::bootstrap::session_graph::{HumanGraphEditAction,HumanGraphEditRequest};
use crate::bootstrap::session_team::SessionTeamSlotEdit;

fn graph_request(fixture:&InputFixture,id:&str,action:HumanGraphEditAction)->HumanGraphEditRequest{
 let snapshot=fixture.controller.snapshot().unwrap();HumanGraphEditRequest{schema_version:1,command_id:CommandId::new(id).unwrap(),session_id:snapshot.owner().session_id.clone(),turn_id:snapshot.turn_id().clone(),execution_epoch_id:snapshot.contract().epochs().last().unwrap().id.clone(),expected_turn_revision:snapshot.contract().revision(),expected_graph_revision:snapshot.contract().graph().unwrap().revision,action,agent:SessionTeamSlotEdit{slot_id:"draft-slot".into(),template_id:Some("controlled-template".into()),source_slot_id:None,role:axocoatl_core::AgentRole::Autonomous,delegation:None,name:"Additional review".into(),provider:"ollama".into(),model:"controlled-model".into(),instructions:None,max_output_tokens:Some(128),writes:None,required:true,reset_history:true,limits:Some(GrantLimits{activations:2,invocations:4,tokens:10000,cost_microunits:0}),expires_at_ms:Some(now_ms().unwrap()+60000),definition:None},task:"Independently verify the accepted parent result".into(),dependencies:if action==HumanGraphEditAction::Add{vec![fixture.parent.input.activation.node_id.clone()]}else{vec![]},replacement:if action==HumanGraphEditAction::Replace{Some(fixture.child.input.activation.node_id.clone())}else{None},rewire_dependents:vec![]}
}
fn captured_definition(fixture:&InputFixture,request:&HumanGraphEditRequest)->(DefinitionSnapshotRef,AgentConfig,ExecutionProfile){
 let identity=request.identity().unwrap();let mut config=fixture.child.config.clone();config.id=AgentId::new(format!("dynamic-conversation-{identity}"));config.tools.clear();config.provider="ollama".into();config.sampling.max_tokens=Some(128);
 let definition_id=AgentDefinitionId::new(format!("dynamic-definition-{identity}")).unwrap();let profile=ExecutionProfile{definition:definition_id.as_str().into(),provider:config.provider.clone(),model:config.model.clone(),isolation:"in-process".into(),tools:vec![],write_scope:None};
 let mut state=fixture.controller.lock().unwrap();let definition=state.content.retain_activation_evidence(ActivationEvidenceContent::Definition{definition_id:definition_id.clone(),revision:1,profile:profile.clone(),configuration:serde_json::to_string(&config).unwrap()}).unwrap().reference().clone();
 let DispatchState{canonical,content,..}=&mut *state;
 // Controlled provider has finite test bounds and performs no network work.
 content.retain_provider_profile(canonical,&definition,"ollama","{}".into()).unwrap();
 (DefinitionSnapshotRef{definition_id,snapshot:definition},config,profile)
}
fn applied_node(fixture:&InputFixture,request:HumanGraphEditRequest)->(InputNode,crate::bootstrap::session_graph::HumanGraphEditPreview){
 let(definition,config,profile)=captured_definition(fixture,&request);
 let before=fixture.controller.snapshot().unwrap().contract().revision();
 let preview=fixture.controller.prepare_human_graph_edit(request.clone(),definition.clone(),None).unwrap();
 assert_eq!(fixture.controller.snapshot().unwrap().contract().revision(),before,"Preview cannot append a graph revision");
 let applied=fixture.controller.prepare_human_graph_edit(request.clone(),definition.clone(),Some(&preview.review_digest)).unwrap();
 let repeated=fixture.controller.prepare_human_graph_edit(request,definition,Some(&preview.review_digest)).unwrap();
 assert_eq!(applied.receipt,repeated.receipt,"exact Apply retry is the original receipt");
 let input=match &applied.receipt.as_ref().unwrap().request.parameters{ControlParameters::AddAgent{input,..}|ControlParameters::ReplaceFutureAgent{input,..}=>(**input).clone(),_=>panic!("wrong control")};
 (InputNode{input,config,profile},applied)
}
#[tokio::test]
async fn current_graph_add_reuses_existing_driver_and_real_parent_acceptance(){
 let fixture=input_fixture();let factory=DriverFactory::new(vec![driver_plan(&fixture.parent,InputProvider::new(PARENT_V1,true,false)),driver_plan(&fixture.child,InputProvider::new(CHILD_V1,false,false))]);
 let driver=fixture.controller.autonomous_turn_driver(driver_seeds(&fixture),factory.clone()).unwrap();
 let request=graph_request(&fixture,"add-current-dependent",HumanGraphEditAction::Add);
 let(node,applied)=applied_node(&fixture,request);assert_eq!(applied.graph.revision,2);
 let provider=InputProvider::named("ollama","new-dependent-accepted",false,false);
 factory.plans.lock().unwrap().insert((node.input.activation.node_id.as_str().into(),1),driver_plan(&node,provider.clone()));
 let outcome=driven(driver).await;assert_eq!(outcome.snapshot.contract().state(),Some(LogicalTurnState::Completed));
 let accepted=outcome.snapshot.contract().current_accepted_activations();assert_eq!(accepted.len(),3);
 let added=accepted.iter().find(|item|item.activation.node_id==node.input.activation.node_id).unwrap();assert_eq!(added.input.parents.len(),1);assert_eq!(added.input.parents[0].activation.node_id,fixture.parent.input.activation.node_id);assert_eq!(provider.calls.load(Ordering::SeqCst),1);
 fixture.controller.lock().unwrap().validate_retained_graph_controls().unwrap();
}
#[tokio::test]
async fn current_graph_replacement_preserves_history_and_never_runs_replaced_work(){
 let fixture=input_fixture();let old_child=InputProvider::new(CHILD_V1,false,false);let factory=DriverFactory::new(vec![driver_plan(&fixture.parent,InputProvider::new(PARENT_V1,true,false)),driver_plan(&fixture.child,old_child.clone())]);
 let driver=fixture.controller.autonomous_turn_driver(driver_seeds(&fixture),factory.clone()).unwrap();let request=graph_request(&fixture,"replace-future-child",HumanGraphEditAction::Replace);let(node,applied)=applied_node(&fixture,request);
 assert_eq!(applied.graph.nodes.len(),2);factory.plans.lock().unwrap().insert((node.input.activation.node_id.as_str().into(),1),driver_plan(&node,InputProvider::named("ollama","replacement-result",false,false)));
 let outcome=driven(driver).await;assert_eq!(outcome.snapshot.contract().state(),Some(LogicalTurnState::Completed));assert_eq!(old_child.calls.load(Ordering::SeqCst),0);assert_eq!(outcome.snapshot.contract().replaced_nodes()[0].previous,fixture.child.input.activation.node_id);assert_eq!(outcome.snapshot.contract().graph_history()[0].previous.nodes.len(),2);
}
#[test]
fn current_graph_refuses_started_replacement_and_stale_revision_without_graph_write(){
 let fixture=input_fixture();start_input(&fixture.controller,&fixture.parent);let mut request=graph_request(&fixture,"cannot-replace-started-parent",HumanGraphEditAction::Replace);request.replacement=Some(fixture.parent.input.activation.node_id.clone());request.rewire_dependents=vec![fixture.child.input.activation.node_id.clone()];let(definition,_,_)=captured_definition(&fixture,&request);let before=fixture.controller.snapshot().unwrap().contract().revision();assert!(fixture.controller.prepare_human_graph_edit(request,definition,None).is_err());assert_eq!(fixture.controller.snapshot().unwrap().contract().revision(),before);
 let mut request=graph_request(&fixture,"stale-add",HumanGraphEditAction::Add);request.expected_turn_revision-=1;let(definition,_,_)=captured_definition(&fixture,&request);assert!(fixture.controller.prepare_human_graph_edit(request,definition,None).is_err());assert_eq!(fixture.controller.snapshot().unwrap().contract().revision(),before);
}
#[test]
fn pending_owner_resolves_exact_applied_graph_receipt_without_runtime_or_provider(){
 let fixture=input_fixture();let request=graph_request(&fixture,"pending-graph-receipt",HumanGraphEditAction::Add);let(_,applied)=applied_node(&fixture,request.clone());
 let state=Arc::try_unwrap(fixture.controller.state).ok().unwrap().into_inner().unwrap();
 let DispatchState{canonical,content,memory,audit,authority,commands,..}=state;
 // The pending owner must reacquire these stores after the live owner is gone.
 drop((memory,audit,authority,commands));
 let receipt=crate::session_dispatch::pending_human_graph_receipt(&canonical,&content,&request,Some(&applied.review_digest)).unwrap().unwrap();
 assert_eq!(receipt.receipt,applied.receipt);assert_eq!(receipt.graph,applied.graph);
 let mut different=request;different.task="Different work under an old command".into();assert!(crate::session_dispatch::pending_human_graph_receipt(&canonical,&content,&different,Some(&applied.review_digest)).is_err());
}
