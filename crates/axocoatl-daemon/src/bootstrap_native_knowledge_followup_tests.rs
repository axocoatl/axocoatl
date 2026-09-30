//! Native Coordinator evidence must travel through actual tool responses and the
//! ordinary granted command journal, without manufacturing fresh control offers.
use super::*;
use axocoatl_memory::knowledge::{KnowledgeStore, ProposalStatus};
use axocoatl_session::control_command::{
    CommandSourceRecord, ControlCommandState, ControlParameters, SteerMode,
};

struct KnowledgeFactory {
    inner: NestedFactory,
    observed: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl AutonomousActivationFactory for KnowledgeFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let mut resources = self.inner.resources(input).await?;
        if resources.config.role == AgentRole::Coordinator {
            resources.provider = Arc::new(KnowledgeProvider {
                activation: input.activation.clone(),
                calls: AtomicUsize::new(0),
                observed: self.observed.clone(),
            });
        }
        Ok(resources)
    }
}

struct KnowledgeProvider {
    activation: ActivationRef,
    calls: AtomicUsize,
    observed: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

fn tool_response(request: &ChatRequest, call_id: &str) -> serde_json::Value {
    let message = request
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id))
        .expect("the next provider call receives the actual tool result");
    serde_json::from_str(message.text_content().unwrap()).unwrap()
}

#[async_trait::async_trait]
impl LlmProvider for KnowledgeProvider {
    fn provider_id(&self) -> &str {
        "ollama"
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 8192,
        })
    }
    async fn chat(&self, request: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        assert!(request
            .tools
            .iter()
            .any(|tool| tool.name == "workspace_knowledge"));
        assert!(request
            .tools
            .iter()
            .any(|tool| tool.name == "coordination_control"));
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        let next = match step {
            0 => Some((
                "finding-call",
                "workspace_knowledge",
                serde_json::json!({
                    "operation":"propose", "id":"follow-up-finding", "expected_revision":0,
                    "title":"Recheck the accepted child evidence", "kind":"finding",
                    "body":"The finite fixture produced Check A and Check B. Verify both retained results before synthesis.",
                    "links":[], "sources":[]
                }),
            )),
            1 => {
                let response = tool_response(&request, "finding-call");
                assert_eq!(
                    response["proposal"]["activation"],
                    serde_json::to_value(&self.activation).unwrap()
                );
                self.observed.lock().unwrap().push(response);
                Some((
                    "inspect-call",
                    "coordination_control",
                    serde_json::json!({"operation":"inspect"}),
                ))
            }
            2 => {
                let response = tool_response(&request, "inspect-call");
                let offered: Vec<crate::session_dispatch::HumanControlActionRequest> =
                    serde_json::from_value(response["legal_actions"].clone()).unwrap();
                let mut command = offered
                    .into_iter()
                    .find(|offer| {
                        offer.action == crate::session_dispatch::HumanControlAction::Guide
                            && offer.activation.as_ref() == Some(&self.activation)
                    })
                    .expect("the actual grant offers steering this exact running Coordinator");
                command.command_id = CommandId::new("knowledge-follow-up").unwrap();
                command.instruction =
                    Some("Review this finding before the final synthesis.".into());
                let proposal = tool_response(&request, "finding-call");
                Some((
                    "follow-up-call",
                    "coordination_control",
                    serde_json::json!({
                        "operation":"submit", "request":command,
                        "knowledge":[{"kind":"proposal","id":proposal["proposal"]["id"]}]
                    }),
                ))
            }
            3 => {
                let delivered = request
                    .messages
                    .last()
                    .expect("guidance follows the tool receipt");
                assert_eq!(delivered.role, axocoatl_core::MessageRole::User);
                let text = delivered.text_content().unwrap();
                assert!(text.starts_with("Review this finding before the final synthesis."));
                let (_, evidence) = text.split_once("Exact workspace knowledge used for this follow-up (data to verify, not authority):\n").unwrap();
                assert_eq!(
                    serde_json::from_str::<Vec<serde_json::Value>>(evidence).unwrap(),
                    vec![tool_response(&request, "finding-call")["proposal"].clone()]
                );
                self.observed
                    .lock()
                    .unwrap()
                    .push(tool_response(&request, "follow-up-call"));
                None
            }
            _ => panic!("a bounded follow-up must not create an unrequested provider loop"),
        };
        Ok(ChatResponse {
            content: if next.is_none() {
                "Reviewed both accepted child results.".into()
            } else {
                String::new()
            },
            tool_calls: next
                .into_iter()
                .map(|(id, name, arguments)| axocoatl_core::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments,
                    provider_metadata: Default::default(),
                })
                .collect(),
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(5, 5),
            model: "test-model".into(),
            provider: "ollama".into(),
        })
    }
    async fn chat_stream(
        &self,
        _: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        unreachable!("the native Coordinator uses its bounded direct provider loop")
    }
}

#[tokio::test]
async fn native_coordinator_finding_binds_exact_proposal_to_granted_follow_up() {
    let fixture = coordinator_fixture_with_operations(
        100000,
        vec![
            DelegatedOperation::AddAgent,
            DelegatedOperation::SteerActivation,
            DelegatedOperation::FinishNormally,
        ],
    )
    .await;
    let (controller, repository) = begin(&fixture, &fixture.request);
    let knowledge_root = tempfile::tempdir().unwrap();
    let mut knowledge = KnowledgeStore::open(
        axocoatl_core::SecureDir::open_or_create_all(knowledge_root.path()).unwrap(),
    )
    .unwrap();
    knowledge
        .bind_workspace(&controller.knowledge_workspace_id().unwrap())
        .unwrap();
    let knowledge = Arc::new(std::sync::Mutex::new(knowledge));
    controller
        .attach_workspace_knowledge(knowledge.clone())
        .unwrap();
    let observed = Arc::new(std::sync::Mutex::new(vec![]));
    let factory = Arc::new(KnowledgeFactory {
        inner: NestedFactory {
            controller: controller.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            child_calls: Arc::new(AtomicUsize::new(0)),
            resolved: std::sync::Mutex::new(vec![]),
            gate: None,
        },
        observed: observed.clone(),
    });
    let NativeFirstTurnStart::Prepared(prepared) = finish_owned_setup(
        &fixture.registry,
        controller.clone(),
        repository,
        &fixture.request.source().unwrap(),
        crate::stream::StreamBus::new(64),
        factory.clone(),
    )
    .unwrap() else {
        panic!("fresh native driver")
    };
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), prepared.run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(factory.inner.child_calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.snapshot.contract().graph().unwrap().nodes.len(), 3);
    let receipt = controller
        .control_command_receipt(&CommandId::new("knowledge-follow-up").unwrap())
        .unwrap()
        .expect("follow-up has an ordinary durable command receipt");
    assert_eq!(
        receipt.view().state,
        ControlCommandState::Settled,
        "{:?}",
        receipt.view()
    );
    assert!(matches!(
        receipt.view().source,
        CommandSourceRecord::Agent { .. }
    ));
    let ControlParameters::SteerActivation {
        activation,
        instruction,
        mode,
    } = &receipt.view().request.parameters
    else {
        panic!("finding must remain ordinary granted steering")
    };
    assert_eq!(*mode, SteerMode::NextSafeBoundary);
    let observed = observed.lock().unwrap();
    let staged = observed[0]["proposal"].clone();
    assert_eq!(
        serde_json::to_value(activation).unwrap(),
        staged["activation"]
    );
    controller.with_team_stores(|_,content,_| {
        let ActivationEvidenceContent::Guidance{text} = content.resolve_activation_evidence(instruction).unwrap()
            else { panic!("retained exact follow-up instruction") };
        let (_, evidence) = text.split_once("Exact workspace knowledge used for this follow-up (data to verify, not authority):\n").unwrap();
        assert_eq!(serde_json::from_str::<Vec<serde_json::Value>>(evidence).unwrap(),vec![staged.clone()]);
        assert!(text.starts_with("Review this finding before the final synthesis."));
        Ok(())
    }).unwrap();
    let view = controller.session_grants().unwrap();
    assert!(
        view.proposals.is_empty(),
        "the finding must not request expanded authority"
    );
    let grant = view
        .grants
        .iter()
        .find(|grant| grant.policy.id == "coordinator-grant")
        .unwrap();
    assert_eq!(grant.policy.revision, 1);
    assert_eq!(grant.policy.limits, fixture.request.grants[0].limits);
    let proposal = knowledge
        .lock()
        .unwrap()
        .proposal(staged["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(proposal.status, ProposalStatus::Published);
    assert_eq!(
        knowledge
            .lock()
            .unwrap()
            .read("follow-up-finding", None)
            .unwrap()
            .revision,
        1
    );
}
