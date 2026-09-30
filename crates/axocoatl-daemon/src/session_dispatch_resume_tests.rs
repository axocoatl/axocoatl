//! Real native actor/hook/controller seam with a finite local provider and tool.
//! The test hook exercises the same opaque callback as the configured MCP hook;
//! it does not replace the controller, receipt store, authority or actor loop.
use super::*;
use axocoatl_session::control_command::ControlCommandState;
use axocoatl_tools::{HookAction, HookContext, HookPhase, ToolHook, SharedHookApprovalBoundary, HookApprovalResolution, HookRegistry};

struct ExactWaitHook { timeout: Duration }
#[async_trait]
impl ToolHook for ExactWaitHook {
    fn name(&self) -> &str { "exact_test_human_wait" }
    fn phases(&self) -> Vec<HookPhase> { vec![HookPhase::Pre] }
    async fn execute(&self, _: &HookContext) -> HookAction { panic!("native actor must install its exact approval boundary") }
    async fn execute_with_approval(&self, context: &HookContext, boundary: SharedHookApprovalBoundary) -> HookAction {
        let mut scoped = context.clone();
        scoped.agent_id = boundary.actor_scope().unwrap();
        let display = serde_json::json!({"approval_id":"display-only", "agent_id":scoped.agent_id,
            "server":"test", "tool":scoped.tool_name, "tool_display":"Count effect",
            "arguments_preview":serde_json::to_string(&scoped.value).unwrap(), "requested_at":1});
        match boundary.request_human_approval(&scoped, display, self.timeout).await {
            Ok(HookApprovalResolution::Approved) => HookAction::Allow,
            Ok(HookApprovalResolution::Denied { reason }) | Err(reason) => HookAction::Deny { reason },
        }
    }
}
struct ChangeApprovedArguments;
#[async_trait]
impl ToolHook for ChangeApprovedArguments {
    fn name(&self) -> &str { "post_approval_transform" }
    fn phases(&self) -> Vec<HookPhase> { vec![HookPhase::Pre] }
    async fn execute(&self, _: &HookContext) -> HookAction {
        HookAction::Transform { value: serde_json::json!({"value":"different executable input"}) }
    }
}
fn install_wait(fixture: &Fixture, timeout: Duration, transform: bool) {
    let mut hooks = HookRegistry::new();
    hooks.register_global(Arc::new(ExactWaitHook { timeout }));
    if transform { hooks.register_global(Arc::new(ChangeApprovedArguments)); }
    fixture.controller.install_hook_registry(Some(Arc::new(hooks))).unwrap();
}
async fn pending_wait(controller: &SessionDispatchController) -> BlockerId {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let changed = controller.lock().unwrap().changed.clone();
            let notified = changed.notified(); tokio::pin!(notified); notified.as_mut().enable();
            if let Some(item) = controller.snapshot().unwrap().contract().blockers().iter().find(|item| item.state == TurnBlockerState::Pending) {
                return item.blocker.blocker_id.clone();
            }
            notified.await;
        }
    }).await.unwrap()
}
fn response(fixture: &Fixture, blocker: BlockerId, id: &str, choice: HumanBlockerResponse) -> HumanControlActionRequest {
    let snapshot = fixture.controller.snapshot().unwrap();
    HumanControlActionRequest { schema_version: 1, command_id: CommandId::new(id).unwrap(),
        session_id: fixture.activation.session_id.clone(), turn_id: fixture.activation.turn_id.clone(),
        execution_epoch_id: fixture.activation.execution_epoch_id.clone(), expected_turn_revision: snapshot.contract().revision(),
        expected_graph_revision: snapshot.contract().graph().unwrap().revision, activation: Some(fixture.activation.clone()),
        action: HumanControlAction::Resume, instruction: None, include_previous_output: false, context: None, continuation: None,
        blocker_id: Some(blocker), human_response: Some(choice), partial_finish: None }
}

#[tokio::test]
async fn native_human_approval_is_exact_idempotent_and_preserves_group_and_accounting() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), false);
    let bus = crate::stream::StreamBus::new(32);
    let mut subscription = bus.subscribe();
    fixture.controller.attach_stream_bus(bus).unwrap();
    let original = fixture.controller.snapshot().unwrap().contract().activations()[0].input.clone();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    let mut invalidations = vec![];
    while let Ok(frame) = subscription.try_recv() {
        if let crate::stream::StreamFrame::ActivationControlChanged { activation, blocker_id, canonical_command_id, turn_revision } = frame {
            assert_eq!(activation, fixture.activation); assert_eq!(blocker_id, blocker);
            let state = fixture.controller.lock().unwrap();
            assert!(state.canonical.records().unwrap().iter().any(|record| record.command_id == canonical_command_id && record.expected_revision + 1 == turn_revision));
            invalidations.push(canonical_command_id);
        }
    }
    assert_eq!(invalidations.len(), 1, "Open is published only after durable persistence");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1); assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let before = fixture.controller.lock().unwrap().canonical.records().unwrap().to_vec();
    let controls = fixture.controller.control_plane().unwrap();
    let choice = &controls.nodes[0].activations[0].capabilities.human_responses[0];
    assert_eq!(choice.blocker_id, blocker); assert!(choice.approve.enabled); assert!(choice.decline.enabled);
    assert_eq!(fixture.controller.lock().unwrap().canonical.records().unwrap(), before);
    let request = response(&fixture, blocker, "approve-native", HumanBlockerResponse::Approval);
    let decoded = HumanControlActionRequest::decode(&serde_json::to_vec(&request).unwrap()).unwrap();
    let first = fixture.controller.submit_human_action(decoded.clone(), 1).unwrap();
    assert_eq!(first.state, ControlCommandState::Applied, "the synchronous API cannot acknowledge hook delivery");
    assert_eq!(fixture.controller.submit_human_action(decoded.clone(), 2).unwrap(), first);
    let mut conflicting = decoded.clone(); conflicting.human_response = Some(HumanBlockerResponse::Decline { reason: "changed answer".into() });
    assert!(fixture.controller.submit_human_action(conflicting, 3).is_err());
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2); assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.controller.submit_human_action(decoded, 4).unwrap().state, ControlCommandState::Settled);
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert_eq!(snapshot.contract().activations()[0].input, original);
    assert_eq!(snapshot.contract().blockers().len(), 1);
    assert!(matches!(snapshot.contract().blockers()[0].state, TurnBlockerState::Resolved { response: TurnBlockerResponse::HumanApproval { .. } }));
    let checkpoint = state.memory.checkpoint(settled.checkpoint.as_ref().unwrap()).unwrap();
    assert_eq!(checkpoint.cumulative_token_usage, TokenUsageStats::new(20, 4));
    assert_eq!(checkpoint.session_messages[1].tool_calls[0].id, "native-call");
    assert_eq!(checkpoint.session_messages[2].tool_call_id.as_deref(), Some("native-call"));
    assert_eq!(snapshot.contract().invocations().len(), 1);
}

#[tokio::test]
async fn native_human_decline_is_a_tool_denial_and_actor_can_choose_another_permitted_action() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), false);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    let request = response(&fixture, blocker, "decline-native", HumanBlockerResponse::Decline { reason: "Use an approach without this effect".into() });
    fixture.controller.submit_human_action(request.clone(), 1).unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2); assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.controller.submit_human_action(request, 2).unwrap().state, ControlCommandState::Settled);
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    assert!(snapshot.contract().invocations().is_empty());
    assert!(matches!(snapshot.contract().blockers()[0].state, TurnBlockerState::Resolved { response: TurnBlockerResponse::HumanDecline { .. } }));
    let checkpoint = state.memory.checkpoint(settled.checkpoint.as_ref().unwrap()).unwrap();
    assert!(checkpoint.session_messages[2].content.contains("Use an approach without this effect"));
    assert_eq!(checkpoint.session_messages[2].tool_call_id.as_deref(), Some("native-call"));
    assert_eq!(checkpoint.cumulative_token_usage, TokenUsageStats::new(20, 4));
}

#[tokio::test]
async fn native_human_wait_timeout_abandons_without_inventing_human_denial() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_millis(1), false);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let settled = tokio::time::timeout(Duration::from_secs(3), fixture.controller.prepare_autonomous_activation(
        fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run()).await.unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0); assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().blockers().len(), 1);
    assert!(matches!(snapshot.contract().blockers()[0].state, TurnBlockerState::Abandoned { .. }));
    assert!(snapshot.contract().invocations().is_empty());
}

#[tokio::test]
async fn whole_stop_cancels_exact_human_wait_and_cannot_resume_or_dispatch_afterward() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), false);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    let request = response(&fixture, blocker, "late-native-response", HumanBlockerResponse::Approval);
    fixture.controller.request_human_turn_stop(fixture.activation.session_id.as_str(), fixture.activation.turn_id.as_str()).unwrap();
    assert!(fixture.controller.submit_human_action(request, 1).is_err());
    assert!(!tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap().accepted);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0); assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().state(), Some(LogicalTurnState::Cancelled));
    assert!(matches!(snapshot.contract().blockers()[0].state, TurnBlockerState::Abandoned { .. }));
    let usage = fixture.controller.activation_provider_usage(&fixture.activation).unwrap();
    assert!(usage.tokens.complete); assert_eq!(usage.tokens.usage, TokenUsageStats::new(10, 2));
}

#[tokio::test]
async fn epoch_loss_and_reopen_preserve_wait_evidence_without_recreating_resumability() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), false);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    let request = response(&fixture, blocker, "lost-native-response", HumanBlockerResponse::Approval);
    let snapshot = fixture.controller.snapshot().unwrap();
    fixture.controller.append_host_event(TurnContractEnvelope { schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new("interrupt-human-wait").unwrap(), expected_revision: snapshot.contract().revision(),
        session_id: fixture.activation.session_id.clone(), turn_id: fixture.activation.turn_id.clone(),
        event: TurnContractEvent::InterruptEpoch { epoch_id: fixture.activation.execution_epoch_id.clone() } }).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap();
    assert_eq!(tool.count.load(Ordering::SeqCst), 0); assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let refused = fixture.controller.submit_human_action(request.clone(), 1);
    assert!(refused.is_err() || refused.unwrap().state == ControlCommandState::Rejected);
    drop(provider);
    let Fixture { _root, ownership, owner, controller, activation, .. } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id).unwrap();
    let snapshot = reopened.snapshot().unwrap();
    assert!(matches!(snapshot.contract().blockers()[0].state, TurnBlockerState::Interrupted { .. }));
    let refused = reopened.submit_human_action(request, 2);
    assert!(refused.is_err() || refused.unwrap().state == ControlCommandState::Rejected);
    assert!(reopened.control_plane().unwrap().nodes[0].activations[0].capabilities.human_responses.iter().all(|item| !item.approve.enabled && !item.decline.enabled));
}

#[tokio::test]
async fn post_approval_transform_cannot_reuse_a_human_decision_for_different_executable_bytes() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), true);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    let request = response(&fixture, blocker, "approve-original-bytes", HumanBlockerResponse::Approval);
    fixture.controller.submit_human_action(request, 1).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap();
    assert!(result.is_err() || !result.unwrap().accepted);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0); assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(fixture.controller.snapshot().unwrap().contract().invocations().is_empty());
}

#[tokio::test]
async fn cancellation_before_native_hook_creates_no_human_wait() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), false);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let prepared = fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap();
    fixture.controller.request_human_turn_stop(fixture.activation.session_id.as_str(), fixture.activation.turn_id.as_str()).unwrap();
    assert!(prepared.run().await.is_err());
    assert!(fixture.controller.snapshot().unwrap().contract().blockers().is_empty());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0); assert_eq!(tool.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn human_response_cannot_authorize_another_provider_call_after_grant_revocation() {
    let fixture = run_fixture(); install_wait(&fixture, Duration::from_secs(60), false);
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    {
        let state = fixture.controller.lock().unwrap();
        let grant = state.bound.get(&fixture.activation.activation_id).unwrap().grant.clone();
        state.authority.revoke_grant(grant.grant_id.as_str(), state.authority.revision().unwrap()).unwrap();
    }
    let request = response(&fixture, blocker.clone(), "approve-after-revoke", HumanBlockerResponse::Approval);
    let rejected = fixture.controller.submit_human_action(request, 1).unwrap();
    assert_eq!(rejected.state, ControlCommandState::Rejected);
    let request = response(&fixture, blocker, "decline-after-revoke", HumanBlockerResponse::Decline { reason: "Do not dispatch this effect".into() });
    fixture.controller.submit_human_action(request, 1).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap();
    assert!(result.is_err() || !result.unwrap().accepted);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0); assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let usage = fixture.controller.activation_provider_usage(&fixture.activation).unwrap();
    assert!(usage.tokens.complete); assert_eq!(usage.tokens.usage, TokenUsageStats::new(10, 2));
    assert!(fixture.controller.snapshot().unwrap().contract().invocations().is_empty());
}

#[test]
fn resume_wire_requires_exact_human_choice_and_preserves_old_action_serialization() {
    let fixture = run_fixture();
    let request = response(&fixture, BlockerId::new("blocker").unwrap(), "response-shape", HumanBlockerResponse::Approval);
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["human_response"], serde_json::json!({"kind":"approval"}));
    assert_eq!(HumanControlActionRequest::decode(&serde_json::to_vec(&value).unwrap()).unwrap(), request);
    for (key, bad) in [("blocker_id", serde_json::Value::Null), ("human_response", serde_json::Value::Null),
        ("human_response", serde_json::json!({"kind":"decline","reason":" "})),
        ("human_response", serde_json::json!({"kind":"machine_evidence","evidence":"not-human"})),
        ("include_previous_output", serde_json::json!(true))]
    {
        let mut malformed = value.clone(); malformed[key] = bad;
        assert!(HumanControlActionRequest::decode(&serde_json::to_vec(&malformed).unwrap()).is_err());
    }
    let mut legacy_shape = request; legacy_shape.action = HumanControlAction::Stop;
    legacy_shape.blocker_id = None; legacy_shape.human_response = None;
    let wire = serde_json::to_value(&legacy_shape).unwrap();
    assert!(wire.get("blocker_id").is_none()); assert!(wire.get("human_response").is_none());
    assert_eq!(HumanControlActionRequest::decode(&serde_json::to_vec(&wire).unwrap()).unwrap(), legacy_shape);
}

#[tokio::test]
async fn native_hook_after_grant_expansion_keeps_immutable_blocker_authority() {
    let fixture = run_fixture();
    install_wait(&fixture, Duration::from_secs(60), false);
    let original = fixture.controller.snapshot().unwrap().contract().activations()[0].input.grant.clone().unwrap();
    {
        let mut state = fixture.controller.lock().unwrap();
        let policy = state.authority.grant_status(original.grant_id.as_str()).unwrap().policy;
        let proposal = crate::session_dispatch::SessionGrantChange {
            request_id: "prior-expansion".into(), activation: fixture.activation.clone(),
            grant_id: policy.id.clone(), expected_grant_revision: policy.revision,
            limits: policy.limits.clone(), expires_at_ms: policy.expires_at_ms,
            operations: vec![], max_nodes: 1, max_edges: 0, reason: "Retained earlier budget review".into(),
        };
        let parameters = state.content.retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: serde_json::json!({"kind":"delegated_grant_expansion_v1","request":proposal,"grant":original}).to_string(),
        }).unwrap().reference().clone();
        let blocker_id = BlockerId::new("grant-proposal-prior-expansion").unwrap();
        let command_id = CommandId::new("open-grant-proposal-prior-expansion").unwrap();
        state.append(command_id.as_str(), TurnContractEvent::OpenBlocker { blocker: TypedTurnBlocker {
            schema_version: 1, blocker_id: blocker_id.clone(), activation: fixture.activation.clone(),
            kind: TurnBlockerKind::HumanApproval { approval_request: parameters.clone() },
            command_id: command_id.clone(), invocation_id: None, grant: Some(original.clone()),
            parameters: parameters.clone(), safe_boundary: parameters.clone(), evidence: parameters.clone(),
        }}).unwrap();
        let mut expanded = policy; expanded.revision += 1; expanded.limits.tokens += 1000;
        let revision = state.authority.revision().unwrap();
        state.authority.approve_expanded_grant(expanded, parameters.clone(), revision).unwrap();
        state.append("resolve-prior-expansion", TurnContractEvent::ResolveBlocker {
            blocker_id, activation: fixture.activation.clone(),
            response: TurnBlockerResponse::HumanApproval { approval_request: parameters.clone(), approval_evidence: parameters },
        }).unwrap();
    }
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let run = tokio::spawn(fixture.controller.prepare_autonomous_activation(fixture.activation.clone(), resources(&fixture, provider.clone(), tool.clone())).unwrap().run());
    let blocker = pending_wait(&fixture.controller).await;
    assert!(blocker.as_str().starts_with("human-wait-"));
    {
        let state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
        let retained = snapshot.contract().blockers().iter().find(|item| item.blocker.blocker_id == blocker).unwrap();
        assert_eq!(retained.blocker.grant, Some(original));
        assert_eq!(state.bound[&fixture.activation.activation_id].grant.revision, 2);
        super::human_wait::validate_retained_human_waits(&snapshot, &state.content).unwrap();
    }
    fixture.controller.submit_human_action(response(&fixture, blocker, "approve-after-expansion", HumanBlockerResponse::Approval), 1).unwrap();
    let settled = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
}
