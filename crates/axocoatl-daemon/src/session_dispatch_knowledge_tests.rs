//! Exercise memory through real native actor/tool admission, not only storage.
use super::*;
use axocoatl_memory::knowledge::{KnowledgeKind, KnowledgeProvenance, KnowledgeStore, ProposalStatus};

struct KnowledgeProvider { calls:AtomicUsize, fail:bool, args:Option<serde_json::Value>, expect:&'static str }
impl KnowledgeProvider {
    fn new(fail:bool)->Self {Self{calls:AtomicUsize::new(0),fail,args:None,expect:"pending"}}
}
#[async_trait]
impl LlmProvider for KnowledgeProvider {
    fn provider_id(&self)->&str {"controlled"}
    fn model_id(&self)->&str {"controlled-model"}
    fn capabilities(&self)->ProviderCapabilities {ProviderCapabilities{streaming:true,tool_calling:true,..Default::default()}}
    fn execution_bounds(&self,_:&ChatRequest)->Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds{token_limit:100,cost_microunits:0,response_bytes:8192})
    }
    async fn chat(&self,_:ChatRequest)->std::result::Result<ChatResponse,ProviderError> {unreachable!()}
    async fn chat_stream(&self,request:ChatRequest)->std::result::Result<Pin<Box<dyn Stream<Item=std::result::Result<StreamEvent,ProviderError>>+Send>>,ProviderError> {
        assert!(request.tools.iter().any(|tool|tool.name=="workspace_knowledge"));
        let round=self.calls.fetch_add(1,Ordering::SeqCst);
        assert!(round<2);
        let mut events=if round==0 {
            let args=self.args.clone().unwrap_or_else(||serde_json::json!({"operation":"propose","id":"retry-rule","expected_revision":0,"title":"Retry rule","body":"A retry must preserve the exact source evidence.","kind":"finding","links":[],"sources":[]}));
            vec![Ok(StreamEvent::ToolCallDelta {index:Some(0),id:"finding-call".into(),name:Some("workspace_knowledge".into()),args_delta:args.to_string()})]
        } else {
            let result=request.messages.iter().find(|m|m.tool_call_id.as_deref()==Some("finding-call")).expect("actual acknowledged tool result");
            assert!(result.text_content().unwrap().contains(self.expect),"{}",result.text_content().unwrap());
            if self.fail {return Ok(Box::pin(tokio_stream::iter(vec![Err(ProviderError::Stream("controlled failure after proposal".into()))])))}
            vec![Ok(StreamEvent::TextDelta{delta:"Finding recorded for review".into()})]
        };
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10,2))));
        events.push(Ok(StreamEvent::Done{finish_reason:if round==0 {FinishReason::ToolUse}else{FinishReason::Stop}}));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

struct KnowledgeFactory {
    parent:InputNode,child:InputNode,provider:Arc<KnowledgeProvider>,store:Arc<Mutex<KnowledgeStore>>,
}
#[async_trait]
impl AutonomousActivationFactory for KnowledgeFactory {
    async fn resources(&self,input:&ActivationInputManifest)->std::result::Result<AutonomousActivationResources,String> {
        let parent=input.activation.node_id==self.parent.input.activation.node_id;
        let node=if parent {&self.parent}else{&self.child};
        let mut resources=input_resources(node,InputProvider::new("child verified finding",false,false));
        if parent {resources.provider=self.provider.clone()} else {
            assert!(self.store.lock().unwrap().list().unwrap().is_empty(),"an accepted activation in an open turn cannot publish workspace knowledge");
            assert_eq!(self.store.lock().unwrap().proposals().unwrap().len(),1);
        }
        Ok(resources)
    }
}

/// Agents whose `tools` list `workspace_knowledge`: only they are offered it.
fn knowledge_fixture()->InputFixture {input_fixture_with_tools(false,&["effect","workspace_knowledge"])}

fn store(fixture:&InputFixture)->Arc<Mutex<KnowledgeStore>> {
    let dir=axocoatl_core::SecureDir::open_or_create(fixture._root.path().join("knowledge-test")).unwrap();
    let mut store=KnowledgeStore::open(dir).unwrap();
    store.bind_workspace("input-workspace").unwrap();
    let store=Arc::new(Mutex::new(store));
    fixture.controller.attach_workspace_knowledge(store.clone()).unwrap();store
}

#[tokio::test]
async fn native_knowledge_proposal_is_audited_and_published_only_after_accepted_closure() {
    let fixture=knowledge_fixture();let store=store(&fixture);
    let factory=Arc::new(KnowledgeFactory {parent:fixture.parent.clone(),child:fixture.child.clone(),provider:Arc::new(KnowledgeProvider::new(false)),store:store.clone()});
    let seeds=[&fixture.parent,&fixture.child].into_iter().map(|node|AutonomousNodeInput{node_id:node.input.activation.node_id.clone(),guidance:node.input.guidance.clone(),attachments:node.input.attachments.clone(),repository:node.input.repository.clone(),budget:node.input.budget.clone(),grant:node.input.grant.clone()}).collect();
    let outcome=fixture.controller.autonomous_turn_driver(seeds,factory).unwrap().run().await.unwrap();
    assert_eq!(outcome.snapshot.contract().state(),Some(LogicalTurnState::Completed));
    let note=store.lock().unwrap().read("retry-rule",None).unwrap();
    assert_eq!(note.revision,1);assert_eq!(note.kind,KnowledgeKind::Finding);
    assert!(matches!(note.provenance,KnowledgeProvenance::Model{..}));
    assert_eq!(store.lock().unwrap().proposals().unwrap()[0].status,ProposalStatus::Published);
    fixture.controller.attach_workspace_knowledge(store.clone()).unwrap();
    assert_eq!(store.lock().unwrap().read("retry-rule",None).unwrap().revision,1,"recovery is idempotent");
    let state=fixture.controller.lock().unwrap();
    assert!(state.canonical.records().unwrap().iter().any(|record|matches!(record.event,TurnContractEvent::RecordOutcome{outcome:InvocationOutcome::Succeeded,..})));
}

#[tokio::test]
async fn native_failed_activation_keeps_knowledge_private() {
    let fixture=knowledge_fixture();let store=store(&fixture);
    start_input(&fixture.controller,&fixture.parent);
    let mut resources=input_resources(&fixture.parent,InputProvider::new("unused",false,false));
    resources.provider=Arc::new(KnowledgeProvider::new(true));
    let result=fixture.controller.prepare_autonomous_activation(fixture.parent.input.activation.clone(),resources).unwrap().run().await.unwrap();
    assert!(!result.accepted);
    assert!(store.lock().unwrap().list().unwrap().is_empty());
    assert_eq!(store.lock().unwrap().proposals().unwrap()[0].status,ProposalStatus::Pending);
    assert!(fixture.controller.scoped_knowledge_tool(&fixture.parent.input.activation).is_err(),"settled activation cannot read or stage new memory");
}

#[tokio::test]
async fn an_omitted_revision_creates_and_never_overwrites_an_existing_note() {
    let fixture=knowledge_fixture();let store=store(&fixture);
    let person=axocoatl_memory::knowledge::KnowledgeDraft{id:"retry-rule".into(),title:"Retry rule".into(),body:"A person's decision.".into(),kind:KnowledgeKind::Decision,links:Vec::new(),sources:Vec::new(),provenance:KnowledgeProvenance::Human{author:None}};
    store.lock().unwrap().save(person,0).unwrap();
    start_input(&fixture.controller,&fixture.parent);
    let mut resources=input_resources(&fixture.parent,InputProvider::new("unused",false,false));
    resources.provider=Arc::new(KnowledgeProvider{calls:AtomicUsize::new(0),fail:false,
        args:Some(serde_json::json!({"operation":"propose","id":"retry-rule","title":"Retry rule","body":"Model restatement.","kind":"finding","links":[],"sources":[]})),
        expect:"already exists at revision 1"});
    fixture.controller.prepare_autonomous_activation(fixture.parent.input.activation.clone(),resources).unwrap().run().await.unwrap();
    assert!(store.lock().unwrap().proposals().unwrap().is_empty(),"nothing is staged over the person's note");
    assert_eq!(store.lock().unwrap().read("retry-rule",None).unwrap().body,"A person's decision.");
}

#[tokio::test]
async fn workspace_knowledge_is_offered_only_to_an_agent_that_lists_it() {
    let fixture=input_fixture();let _store=store(&fixture);
    start_input(&fixture.controller,&fixture.parent);
    let provider=InputProvider::new("answered without knowledge",false,false);
    let prepared=fixture.controller.prepare_autonomous_activation(fixture.parent.input.activation.clone(),input_resources(&fixture.parent,provider)).unwrap();
    assert!(fixture.controller.scoped_knowledge_tool(&fixture.parent.input.activation).unwrap().is_none(),"tools is an exact allowlist");
    assert!(prepared.run().await.unwrap().accepted);

    let fixture=knowledge_fixture();let _store=store(&fixture);
    start_input(&fixture.controller,&fixture.parent);
    let provider=InputProvider::new("unused",false,false);
    let _prepared=fixture.controller.prepare_autonomous_activation(fixture.parent.input.activation.clone(),input_resources(&fixture.parent,provider)).unwrap();
    let tool=fixture.controller.scoped_knowledge_tool(&fixture.parent.input.activation).unwrap().expect("listed");
    let description=tool.description();
    assert!(description.contains("Do not use this to report your work or your final answer"),"{description}");
    assert!(!description.contains("should not change"),"{description}");
}
