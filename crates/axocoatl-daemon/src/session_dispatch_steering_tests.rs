use super::*;
use axocoatl_session::execution_content::GuidanceDelivery;
use axocoatl_session::session_history::{SessionHistory, SessionHistoryEntry};

fn history_fixture() -> Fixture {
    fixture_with_captured_input_and_history(
        GrantLimits { activations: 8, invocations: 32, tokens: 1000, cost_microunits: 1000 },
        "in-process",
        AgentConfig { id: AgentId::new("conversation"), name: "Counter".into(),
            provider: "controlled".into(), model: "controlled-model".into(),
            tools: vec!["effect".into()], ..Default::default() },
        "count once", |_, _| vec![], true,
    )
}

fn history_guidance(history: &SessionHistory) -> &axocoatl_session::execution_content::ExecutionGuidanceView {
    let Some(SessionHistoryEntry::ExecutionV2(turn)) = history.get("turn") else { panic!("exact native history") };
    assert_eq!(turn.activations.len(), 1);
    assert_eq!(turn.activations[0].guidance.len(), 1);
    &turn.activations[0].guidance[0]
}

fn guidance_request(fixture: &Fixture, id: &str, text: &str) -> ControlCommandRequest {
    let reference = fixture.controller.lock().unwrap().content.retain_activation_evidence(ActivationEvidenceContent::Guidance { text: text.into() }).unwrap().reference().clone();
    command_request(&fixture.controller, id, ControlParameters::SteerActivation {
        activation: fixture.activation.clone(), instruction: reference, mode: SteerMode::NextSafeBoundary,
    })
}

struct SteeringProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
    first: Mutex<Option<tokio::sync::mpsc::Receiver<std::result::Result<StreamEvent, ProviderError>>>>,
    started: tokio::sync::Notify,
}
#[async_trait]
impl LlmProvider for SteeringProvider {
    fn provider_id(&self) -> &str { "controlled" }
    fn model_id(&self) -> &str { "controlled-model" }
    fn capabilities(&self) -> ProviderCapabilities { ProviderCapabilities { streaming: true, ..Default::default() } }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds { token_limit: 100, cost_microunits: 100, response_bytes: 8192 })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> { unreachable!() }
    async fn chat_stream(&self, request: ChatRequest) -> std::result::Result<Pin<Box<dyn Stream<Item=std::result::Result<StreamEvent, ProviderError>> + Send>>, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(call < 2);
        self.started.notify_one();
        if call == 0 {
            return Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(self.first.lock().unwrap().take().unwrap())));
        }
        Ok(Box::pin(tokio_stream::iter(vec![Ok(StreamEvent::TextDelta { delta: "Revised same-generation answer".into() }),
            Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))), Ok(StreamEvent::Done { finish_reason: FinishReason::Stop })])))
    }
}
fn steering_provider() -> (Arc<SteeringProvider>, tokio::sync::mpsc::Sender<std::result::Result<StreamEvent, ProviderError>>) {
    let (send, receive) = tokio::sync::mpsc::channel(8);
    (Arc::new(SteeringProvider { calls: AtomicUsize::new(0), requests: Mutex::new(vec![]), first: Mutex::new(Some(receive)), started: tokio::sync::Notify::new() }), send)
}
fn steering_resources(fixture: &Fixture, provider: Arc<SteeringProvider>) -> AutonomousActivationResources {
    AutonomousActivationResources { config: fixture.config.clone(), profile: fixture.profile.clone(), provider,
        counter: Arc::new(Counter), tools: Arc::new(axocoatl_tools::ToolExecutor::new()) }
}

#[tokio::test]
async fn steering_during_final_stream_is_delivered_once_with_original_answer_and_usage() {
    let fixture = history_fixture();
    let original = fixture.controller.snapshot().unwrap().contract().activations()[0].input.clone();
    let (provider, send) = steering_provider();
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), steering_resources(&fixture, provider.clone())).unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await.unwrap();
    send.send(Ok(StreamEvent::TextDelta { delta: "Completed prior answer".into() })).await.unwrap();
    let finish = command_request(&fixture.controller, "finish-after-stream", ControlParameters::FinishTurn { mode: FinishMode::Normal });
    assert_eq!(fixture.controller.submit_control_command(finish, human_source(&fixture.controller)).unwrap().view().state, ControlCommandState::Accepted);
    let request = guidance_request(&fixture, "guide-final-stream", "Clarify the result without dropping the prior answer");
    let receipt = fixture.controller.submit_control_command(request.clone(), human_source(&fixture.controller)).unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Accepted);
    assert_eq!(fixture.controller.submit_control_command(request.clone(), human_source(&fixture.controller)).unwrap().view(), receipt.view());
    assert!(fixture.controller.snapshot().unwrap().contract().guidance().is_empty());
    send.send(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2)))).await.unwrap();
    send.send(Ok(StreamEvent::Done { finish_reason: FinishReason::Stop })).await.unwrap();
    drop(send);
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let replay = fixture.controller.submit_control_command(request, human_source(&fixture.controller)).unwrap();
    assert_eq!(replay.view().state, ControlCommandState::Settled);
    let history = fixture.controller.history_snapshot().unwrap();
    assert!(matches!(history_guidance(&history).delivery, GuidanceDelivery::Delivered { .. }));
    assert!(matches!(&history_guidance(&history).instruction, axocoatl_session::execution_content::ContentResolution::Available { content, .. }
        if content == "Clarify the result without dropping the prior answer"));
    let json = history.export_json(axocoatl_session::session_history::HistoryVisibility::Visible).unwrap();
    assert!(json.contains("Clarify the result without dropping the prior answer"));
    assert!(json.contains("\"status\": \"delivered\""));
    assert!(history.export_markdown().contains("Actor input append acknowledged"));
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let pure = state.content.project(&snapshot).unwrap();
    assert_eq!(pure.activations[0].guidance[0].delivery, GuidanceDelivery::HandoffRecorded);
    // A closed historical read joins protected receipts without replay or writes.
    let before = state.canonical.records().unwrap().to_vec();
    let commands_before = state.commands.records().unwrap().to_vec();
    let historical = SessionHistory::from_upgraded(&state.canonical, &state.content).unwrap();
    assert!(matches!(history_guidance(&historical).delivery, GuidanceDelivery::Delivered { .. }));
    assert_eq!(state.canonical.records().unwrap(), before);
    assert_eq!(state.commands.records().unwrap(), commands_before);
    assert_eq!(snapshot.contract().activations().len(), 1);
    assert_eq!(snapshot.contract().activations()[0].input, original);
    assert_eq!(snapshot.contract().guidance().len(), 1);
    let checkpoint = state.memory.checkpoint(settled.checkpoint.as_ref().unwrap()).unwrap();
    assert_eq!(checkpoint.cumulative_token_usage, TokenUsageStats::new(20, 4));
    assert!(checkpoint.cumulative_token_usage_known);
    let messages = checkpoint.session_messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>();
    assert_eq!(messages, ["count once", "Completed prior answer", "Clarify the result without dropping the prior answer", "Revised same-generation answer"]);
    let second = serde_json::to_string(&provider.requests.lock().unwrap()[1].messages).unwrap();
    assert_eq!(second.matches("Clarify the result without dropping the prior answer").count(), 1);
    assert!(second.contains("Completed prior answer"));
}

#[tokio::test]
async fn guidance_during_tool_waits_for_complete_native_group_and_keeps_effect_accounting() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(CountingTool { release: Some(release.clone()), ..Default::default() });
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), tool.started.notified()).await.unwrap();
    let request = guidance_request(&fixture, "guide-tool", "Check the complete tool result");
    let receipt = fixture.controller.submit_control_command(request.clone(), human_source(&fixture.controller)).unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Accepted);
    assert!(fixture.controller.snapshot().unwrap().contract().guidance().is_empty());
    release.notify_one();
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.controller.control_command_receipt(&request.command_id).unwrap().unwrap().view().state, ControlCommandState::Settled);
    let state = fixture.controller.lock().unwrap();
    let checkpoint = state.memory.checkpoint(settled.checkpoint.as_ref().unwrap()).unwrap();
    assert_eq!(checkpoint.cumulative_token_usage, TokenUsageStats::new(20, 4));
    let messages = &checkpoint.session_messages;
    let guide = messages.iter().position(|message| message.content == "Check the complete tool result").unwrap();
    assert!(guide >= 3);
    assert_eq!(messages[guide-1].tool_call_id.as_deref(), Some("native-call"));
    assert_eq!(messages[guide-2].tool_calls[0].id, "native-call");
}

#[tokio::test]
async fn stop_before_guidance_boundary_retains_failed_receipt_without_input_or_replay() {
    let fixture = run_fixture();
    let (provider, _send) = steering_provider();
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), steering_resources(&fixture, provider.clone())).unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(3), provider.started.notified()).await.unwrap();
    let request = guidance_request(&fixture, "guide-stopped", "Do not silently carry this into Retry");
    fixture.controller.submit_control_command(request.clone(), human_source(&fixture.controller)).unwrap();
    fixture.controller.submit_control_command(stop_request(&fixture, "stop-before-guide"), human_source(&fixture.controller)).unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(!settled.accepted);
    let receipt = fixture.controller.control_command_receipt(&request.command_id).unwrap().unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Failed);
    assert!(matches!(receipt.view().last_transition.as_ref(), Some(ControlTransition::Failed { failure }) if failure.code == "steer_not_delivered"));
    assert!(fixture.controller.snapshot().unwrap().contract().guidance().is_empty());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(!fixture.controller.activation_provider_usage(&fixture.activation).unwrap().tokens.complete);
}

#[test]
fn applied_guidance_without_actor_ack_is_unknown_on_restart_and_never_settled_by_replay() {
    let fixture = history_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), Arc::new(CountingTool::default()))).unwrap();
    let request = guidance_request(&fixture, "guide-crash-cut", "Unacknowledged input");
    fixture.controller.submit_control_command(request.clone(), human_source(&fixture.controller)).unwrap();
    let handoff = fixture.controller.take_activation_guidance(&fixture.activation, false).unwrap().unwrap();
    assert_eq!(fixture.controller.control_command_receipt(&request.command_id).unwrap().unwrap().view().state, ControlCommandState::Applied);
    let history = fixture.controller.history_snapshot().unwrap();
    assert_eq!(history_guidance(&history).delivery, GuidanceDelivery::HandoffRecorded);
    assert!(!history.export_markdown().contains("Actor input append acknowledged"));
    drop(handoff); // Canonical handoff exists, actual actor append never happened.
    drop(prepared);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let Fixture { _root, ownership, owner, controller, activation, .. } = fixture;
    drop(provider); drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id).unwrap();
    let receipt = reopened.control_command_receipt(&request.command_id).unwrap().unwrap();
    assert_eq!(receipt.view().state, ControlCommandState::Failed);
    assert!(matches!(receipt.view().last_transition.as_ref(), Some(ControlTransition::Failed { failure }) if failure.code == "steer_delivery_unknown"));
    assert_eq!(reopened.snapshot().unwrap().contract().guidance().len(), 1);
    let history = reopened.history_snapshot().unwrap();
    assert!(matches!(history_guidance(&history).delivery, GuidanceDelivery::Unknown { .. }));
    let hits = history.search("Unacknowledged input");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.turn_id(), "turn");
    assert_eq!(hits[0].matched_fields, vec![axocoatl_session::TurnSearchField::Context]);
    assert!(history.export_markdown().contains("Unacknowledged input"));
    assert!(!history.export_markdown().contains("Actor input append acknowledged"));
}

#[test]
fn final_empty_boundary_closes_admission_before_canonical_acceptance() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider, Arc::new(CountingTool::default()))).unwrap();
    assert!(fixture.controller.take_activation_guidance(&fixture.activation, true).unwrap().is_none());
    let request = guidance_request(&fixture, "guide-after-final-cut", "Too late");
    assert_eq!(fixture.controller.submit_control_command(request, human_source(&fixture.controller)).unwrap().view().state, ControlCommandState::Rejected);
    assert!(fixture.controller.snapshot().unwrap().contract().guidance().is_empty());
    drop(prepared);
}

#[test]
fn accepted_guidance_reserves_existing_input_capacity_and_revocation_or_agent_source_refuses() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), Arc::new(CountingTool::default()))).unwrap();
    let snapshot = fixture.controller.snapshot().unwrap();
    let input = &snapshot.contract().activations()[0].input;
    let available = MAX_INPUT_REFERENCES - input.guidance.len() - input.parents.len() - input.attachments.len();
    for index in 0..available {
        let request = guidance_request(&fixture, &format!("guide-capacity-{index}"), "Retained bounded guidance");
        assert_eq!(fixture.controller.submit_control_command(request, human_source(&fixture.controller)).unwrap().view().state, ControlCommandState::Accepted);
    }
    let request = guidance_request(&fixture, "guide-capacity-overflow", "Cannot evict older guidance");
    assert_eq!(fixture.controller.submit_control_command(request, human_source(&fixture.controller)).unwrap().view().state, ControlCommandState::Rejected);
    assert!(fixture.controller.snapshot().unwrap().contract().guidance().is_empty());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    drop(prepared);

    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider, Arc::new(CountingTool::default()))).unwrap();
    let source = {
        let state = fixture.controller.lock().unwrap();
        state.authority.attest_control_source(&state.bound[&fixture.activation.activation_id].lease, now_ms().unwrap()).unwrap()
    };
    let request = guidance_request(&fixture, "agent-cannot-mint-guide", "Not human source");
    assert_eq!(fixture.controller.submit_control_command(request, source).unwrap().view().state, ControlCommandState::Rejected);
    {
        let state = fixture.controller.lock().unwrap();
        state.authority.revoke_grant("grant", state.authority.revision().unwrap()).unwrap();
    }
    let request = guidance_request(&fixture, "guide-revoked", "Human input cannot reopen execution authority");
    assert_eq!(fixture.controller.submit_control_command(request, human_source(&fixture.controller)).unwrap().view().state, ControlCommandState::Rejected);
    drop(prepared);
}

#[test]
fn acknowledged_input_after_stop_is_not_permission_for_another_provider_call() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), Arc::new(CountingTool::default()))).unwrap();
    let request = guidance_request(&fixture, "guide-stop-during-handoff", "Already handed to actor");
    fixture.controller.submit_control_command(request.clone(), human_source(&fixture.controller)).unwrap();
    let delivery = fixture.controller.take_activation_guidance(&fixture.activation, false).unwrap().unwrap();
    fixture.controller.submit_control_command(stop_request(&fixture, "stop-after-handoff"), human_source(&fixture.controller)).unwrap();
    // Model the actor's real synchronous append, then its one-use acknowledgement.
    let mut actual_input = axocoatl_memory::session::SessionMemory::new();
    actual_input.append(MessageRole::User, &delivery.text, 4);
    delivery.acknowledgement.acknowledge().unwrap();
    assert_eq!(fixture.controller.control_command_receipt(&request.command_id).unwrap().unwrap().view().state, ControlCommandState::Settled);
    assert!(fixture.controller.take_activation_guidance(&fixture.activation, false).unwrap().is_none());
    assert_eq!(actual_input.messages()[0].content, "Already handed to actor");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    drop(prepared);
}

#[test]
fn canonical_guidance_preserves_initial_input_and_rejects_rebound_or_interrupted_delivery() {
    let fixture = run_fixture();
    let snapshot = fixture.controller.snapshot().unwrap();
    let mut fold = snapshot.contract().clone();
    let original = fold.activations()[0].input.clone();
    assert!(serde_json::to_value(&fold).unwrap().get("guidance").is_none());
    let event = TurnContractEnvelope { schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new("guidance-handoff-record").unwrap(), expected_revision: fold.revision(),
        session_id: fixture.activation.session_id.clone(), turn_id: fixture.activation.turn_id.clone(),
        event: TurnContractEvent::ApplyGuidance { activation: fixture.activation.clone(),
            control_command_id: CommandId::new("retained-control-command").unwrap(),
            instruction: EvidenceRef::new("retained-instruction").unwrap(), request: snapshot.request_ref().unwrap().clone() },
    };
    let encoded = serde_json::to_vec(&event).unwrap();
    let decoded = TurnContractEnvelope::decode(&encoded).unwrap();
    assert_eq!(decoded, event);
    assert!(fold.apply(&decoded).unwrap());
    assert!(!fold.apply(&decoded).unwrap());
    assert_eq!(fold.activations()[0].input, original);
    assert_eq!(fold.activations()[0].state, ActivationState::Running);
    let mut conflicting = event.clone(); conflicting.command_id = CommandId::new("duplicate-guidance-control").unwrap();
    conflicting.expected_revision = fold.revision();
    assert!(fold.apply(&conflicting).is_err());
    let before = fold.clone();
    if let TurnContractEvent::ApplyGuidance { activation, control_command_id, .. } = &mut conflicting.event {
        activation.generation += 1; *control_command_id = CommandId::new("another-control").unwrap();
    }
    assert!(fold.apply(&conflicting).is_err());
    assert_eq!(fold, before);
    let interruption = TurnContractEnvelope { command_id: CommandId::new("guidance-lost-epoch").unwrap(), expected_revision: fold.revision(),
        event: TurnContractEvent::InterruptEpoch { epoch_id: fixture.activation.execution_epoch_id.clone() }, ..event.clone() };
    fold.apply(&interruption).unwrap();
    let mut late = event; late.command_id = CommandId::new("late-guidance-handoff").unwrap(); late.expected_revision = fold.revision();
    assert!(fold.apply(&late).is_err());
    assert_eq!(fold.guidance().len(), 1);
}

#[test]
fn guidance_history_refuses_foreign_receipts_and_omits_unrecorded_amendments() {
    let fixture = history_fixture();
    let initial = fixture.controller.history_snapshot().unwrap();
    let Some(SessionHistoryEntry::ExecutionV2(turn)) = initial.get("turn") else { panic!("exact native history") };
    assert!(serde_json::to_value(&turn.activations[0]).unwrap().get("guidance").is_none());
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider, Arc::new(CountingTool::default()))).unwrap();
    let request = guidance_request(&fixture, "guidance-history-owner", "Exact retained instruction");
    fixture.controller.submit_control_command(request, human_source(&fixture.controller)).unwrap();
    let delivery = fixture.controller.take_activation_guidance(&fixture.activation, false).unwrap().unwrap();
    let mut actual_input = axocoatl_memory::session::SessionMemory::new();
    actual_input.append(MessageRole::User, &delivery.text, 4);
    delivery.acknowledgement.acknowledge().unwrap();
    assert!(matches!(history_guidance(&fixture.controller.history_snapshot().unwrap()).delivery, GuidanceDelivery::Delivered { .. }));
    let foreign = history_fixture(); // Identical readable IDs, different canonical incarnation.
    let state = fixture.controller.lock().unwrap();
    let other = foreign.controller.lock().unwrap();
    let before = state.canonical.records().unwrap().to_vec();
    let commands_before = state.commands.records().unwrap().to_vec();
    assert!(other.commands.read_owned_views(&state.canonical, &state.turn_id).is_err());
    let history = SessionHistory::from_upgraded_with_commands(&state.canonical, &state.content, &state.turn_id, &other.commands).unwrap();
    assert!(matches!(history_guidance(&history).delivery, GuidanceDelivery::Unknown { .. }));
    assert!(history.export_markdown().contains("Exact retained instruction"));
    assert!(!history.export_markdown().contains("Actor input append acknowledged"));
    assert_eq!(state.canonical.records().unwrap(), before);
    assert_eq!(state.commands.records().unwrap(), commands_before);
    drop(other); drop(state); drop(prepared);
}

#[test]
fn guidance_history_preserves_missing_instruction_without_inventing_delivery() {
    let fixture = history_fixture();
    {
        let mut state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        let event = TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("historical-guidance-missing-body").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: fixture.activation.session_id.clone(),
            turn_id: fixture.activation.turn_id.clone(),
            event: TurnContractEvent::ApplyGuidance {
                activation: fixture.activation.clone(),
                control_command_id: CommandId::new("missing-command-body").unwrap(),
                instruction: EvidenceRef::new("missing-instruction-body").unwrap(),
                request: EvidenceRef::new("missing-request-body").unwrap(),
            },
        };
        state.canonical.append(event).unwrap();
    }
    let history = fixture.controller.history_snapshot().unwrap();
    let item = history_guidance(&history);
    assert!(matches!(&item.instruction, axocoatl_session::execution_content::ContentResolution::Missing { reference }
        if reference.as_str() == "missing-instruction-body"));
    assert!(matches!(&item.delivery, GuidanceDelivery::Unknown { .. }));
    assert!(history.export_markdown().contains("Instruction body is missing: `missing-instruction-body`"));
    assert!(!history.export_markdown().contains("Actor input append acknowledged"));
    assert!(history.search("missing-instruction-body").is_empty());
    assert!(history.search("missing-command-body").is_empty());
}

#[tokio::test]
async fn exact_human_guide_reaches_actor_and_replays_its_owned_receipt() {
    let fixture = history_fixture();
    assert!(!fixture.controller.lock().unwrap().human_control_capabilities(&fixture.activation, 1).unwrap().guide.enabled);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture.controller.prepare_autonomous_activation(
        fixture.activation.clone(), resources(&fixture, provider.clone(), Arc::new(CountingTool::default())),
    ).unwrap();
    let request = {
        let state = fixture.controller.lock().unwrap();
        let before = state.canonical.records().unwrap().to_vec();
        let commands_before = state.commands.records().unwrap().to_vec();
        assert!(state.human_control_capabilities(&fixture.activation, 1).unwrap().guide.enabled);
        assert_eq!(state.canonical.records().unwrap(), before);
        assert_eq!(state.commands.records().unwrap(), commands_before);
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        HumanControlActionRequest {
            schema_version: 1, command_id: CommandId::new("human-guide-seam").unwrap(),
            session_id: fixture.activation.session_id.clone(), turn_id: fixture.activation.turn_id.clone(),
            execution_epoch_id: fixture.activation.execution_epoch_id.clone(),
            expected_turn_revision: snapshot.contract().revision(),
            expected_graph_revision: snapshot.contract().graph().unwrap().revision,
            activation: Some(fixture.activation.clone()), action: HumanControlAction::Guide,
            blocker_id: None,
            human_response: None,
            partial_finish: None,
            instruction: Some("Keep the exact retained QA target".into()), include_previous_output: false, context: None, continuation: None,
        }
    };
    let decoded = HumanControlActionRequest::decode(&serde_json::to_vec(&request).unwrap()).unwrap();
    let accepted = fixture.controller.submit_human_action(decoded.clone(), 1).unwrap();
    assert_eq!(accepted.state, ControlCommandState::Accepted);
    assert_eq!(fixture.controller.submit_human_action(decoded.clone(), 2).unwrap(), accepted);
    let mut conflicting = decoded.clone(); conflicting.instruction = Some("Another instruction".into());
    assert!(fixture.controller.submit_human_action(conflicting, 3).is_err());
    let mut invalid = decoded.clone(); invalid.include_previous_output = true;
    assert!(HumanControlActionRequest::decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
    let mut invalid = decoded.clone(); invalid.instruction = Some("  ".into());
    assert!(HumanControlActionRequest::decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
    let result = prepared.run().await.unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let receipt = fixture.controller.submit_human_action(decoded, 4).unwrap();
    assert_eq!(receipt.state, ControlCommandState::Settled);
    let history = fixture.controller.history_snapshot().unwrap();
    assert!(matches!(history_guidance(&history).delivery, GuidanceDelivery::Delivered { .. }));
    assert!(history.export_markdown().contains("Keep the exact retained QA target"));
    let hits = history.search("Keep the exact retained QA target");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.turn_id(), "turn");
    assert_eq!(hits[0].matched_fields, vec![axocoatl_session::TurnSearchField::Context]);
    assert!(!fixture.controller.lock().unwrap().human_control_capabilities(&fixture.activation, 5).unwrap().guide.enabled);
}

#[tokio::test]
async fn human_guide_retains_typed_context_and_delivers_exact_image_bytes() {
    use crate::session_dispatch::human_context::{HumanControlContext,PreparedHumanControlContext};
    let fixture=history_fixture();
    let provider=Arc::new(RunProvider::new(&fixture,RunProviderMode::Answer,true));
    let prepared=fixture.controller.prepare_autonomous_activation(fixture.activation.clone(),resources(&fixture,provider.clone(),Arc::new(CountingTool::default()))).unwrap();
    let snapshot=fixture.controller.snapshot().unwrap();
    let context=HumanControlContext{references:Vec::new(),attachment_ids:vec!["review-image".into()]};
    let request=HumanControlActionRequest{schema_version:1,command_id:CommandId::new("image-guide").unwrap(),session_id:fixture.activation.session_id.clone(),turn_id:fixture.activation.turn_id.clone(),execution_epoch_id:fixture.activation.execution_epoch_id.clone(),expected_turn_revision:snapshot.contract().revision(),expected_graph_revision:snapshot.contract().graph().unwrap().revision,activation:Some(fixture.activation.clone()),action:HumanControlAction::Guide,instruction:Some("Inspect the attached screenshot".into()),include_previous_output:false,context:Some(context.clone()),continuation:None,blocker_id:None,human_response:None,partial_finish:None};
    assert!(fixture.controller.submit_human_action(request.clone(),1).is_err(),"Unresolved client references cannot manufacture attachment evidence");
    let image=axocoatl_core::AgentAttachment{id:"review-image".into(),name:"review.png".into(),mime:"image/png".into(),bytes:vec![1,2,3],size:3,extracted_text:None};
    let captured=PreparedHumanControlContext{original:context,references:Vec::new(),attachments:vec![image]};
    let receipt=fixture.controller.submit_human_action_with_context(request.clone(),2,Some(captured)).unwrap();
    assert_eq!(receipt.state,ControlCommandState::Accepted);
    assert_eq!(fixture.controller.submit_human_action(request.clone(),3).unwrap(),receipt,"Exact retry does not re-read attachment state");
    let outcome=prepared.run().await.unwrap();assert!(outcome.accepted,"{:?}",outcome.failure);
    let actual=serde_json::to_string(&provider.requests.lock().unwrap()[0]).unwrap();
    assert!(actual.contains("data:image/png;base64,AQID"),"{actual}");
    assert!(actual.contains("Inspect the attached screenshot"));
    let receipt=fixture.controller.submit_human_action(request,4).unwrap();assert_eq!(receipt.state,ControlCommandState::Settled);
}
