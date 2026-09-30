use super::*;
use axocoatl_session::execution_content::ActivationStreamPayload;

#[tokio::test]
async fn thousands_of_raw_reasoning_deltas_complete_through_the_durable_product_boundary() {
    let fixture = fixture_with_limits(GrantLimits {
        activations: 8, invocations: 32, tokens: 10_000, cost_microunits: 1000,
    }, "in-process");
    let bus = crate::stream::StreamBus::new(128);
    let mut subscription = bus.subscribe();
    fixture.controller.attach_stream_bus(bus).unwrap();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::FragmentedReasoning, true));
    let settled = fixture.controller.prepare_autonomous_activation(
        fixture.activation.clone(),
        resources(&fixture, provider, Arc::new(CountingTool::default())),
    ).unwrap().run().await.unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let frames = state.content.activation_stream(&snapshot, &fixture.activation).unwrap();
    assert!(frames.len() < 64, "wire fragmentation cannot consume one journal record per token");
    let reasoning = frames.iter().filter_map(|frame| match &frame.content.payload {
        ActivationStreamPayload::ReasoningSummary { delta } => Some(delta.as_str()), _ => None,
    }).collect::<String>();
    assert_eq!(reasoning, "x".repeat(5000));
    let mut published = Vec::new();
    while let Ok(frame) = subscription.try_recv() {
        if let crate::stream::StreamFrame::ActivationStream { event } = frame { published.push(event); }
    }
    assert_eq!(published, frames, "publication uses exactly acknowledged durable frames");
    let checkpoint = state.memory.checkpoint(settled.checkpoint.as_ref().unwrap()).unwrap();
    assert_eq!(checkpoint.cumulative_token_usage, TokenUsageStats::new(10, 5002));
    assert!(checkpoint.cumulative_token_usage_known);
    assert_eq!(snapshot.contract().activations()[0].state, ActivationState::Accepted);
}

#[tokio::test]
async fn recovered_stopped_turn_accepts_host_stream_bus_without_reopening_dispatch() {
    let fixture = run_fixture();
    fixture.controller.request_human_turn_stop(
        fixture.activation.session_id.as_str(),
        fixture.activation.turn_id.as_str(),
    ).unwrap();
    assert_eq!(fixture.controller.snapshot().unwrap().contract().state(), Some(LogicalTurnState::Cancelled));
    let Fixture { controller, ownership, owner, activation, _root, .. } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let controller = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let before = controller.snapshot().unwrap();
    controller.attach_stream_bus(crate::stream::StreamBus::new(16)).unwrap();
    let after = controller.snapshot().unwrap();
    assert_eq!(after.contract().revision(), before.contract().revision());
    assert_eq!(after.contract().state(), Some(LogicalTurnState::Cancelled));
    assert!(after.contract().stop_requested().is_some());
    assert!(controller.lock().unwrap().execution_admission().is_err(), "historical Stop remains an actual dispatch fence");
    assert!(controller.live_owned_turn().unwrap().is_none());
    let retired = run_fixture();
    retired.controller.close_registered_repository_admission().unwrap();
    assert!(retired.controller.attach_stream_bus(crate::stream::StreamBus::new(16)).unwrap_err().to_string().contains("lifecycle"));
}

#[tokio::test]
async fn native_actor_stream_is_exact_persisted_before_publication_and_survives_reopen() {
    let fixture = run_fixture();
    let bus = crate::stream::StreamBus::new(16);
    let mut subscription = bus.subscribe();
    fixture.controller.attach_stream_bus(bus).unwrap();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let mut frames = vec![];
    while let Ok(frame) = subscription.try_recv() {
        if let crate::stream::StreamFrame::ActivationStream { event } = frame {
            frames.push(event);
        }
    }
    assert_eq!(frames.len(), 3);
    assert!(matches!(
        frames[0].content.payload,
        ActivationStreamPayload::ToolProposed { .. }
    ));
    assert!(matches!(
        frames[1].content.payload,
        ActivationStreamPayload::ToolResult { .. }
    ));
    assert!(matches!(
        frames[2].content.payload,
        ActivationStreamPayload::Text { .. }
    ));
    let snapshot = fixture.controller.snapshot().unwrap();
    for (sequence, frame) in frames.iter().enumerate() {
        assert_eq!(frame.content.activation, fixture.activation);
        assert_eq!(frame.content.sequence, sequence as u64);
    }
    assert_eq!(
        fixture
            .controller
            .lock()
            .unwrap()
            .content
            .activation_stream(&snapshot, &fixture.activation)
            .unwrap(),
        frames
    );
    let public = serde_json::to_string(&fixture.controller.control_plane().unwrap()).unwrap();
    assert!(public.contains("tool_proposed"));
    assert!(public.contains("stream_text"));
    assert!(
        !public.contains("\"arguments\":{\"value\":\"actual\"}"),
        "protected arguments are not stream display data"
    );
    drop(provider);
    let Fixture {
        controller,
        ownership,
        owner,
        activation,
        _root,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let controller =
        SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let reopened = controller.snapshot().unwrap();
    assert_eq!(
        controller
            .lock()
            .unwrap()
            .content
            .activation_stream(&reopened, &activation)
            .unwrap(),
        frames
    );
    assert!(controller.control_plane().unwrap().nodes[0].activations[0]
        .evidence
        .iter()
        .any(|item| item.kind == "stream_text"));
}

#[tokio::test]
async fn failed_stream_acknowledgement_publishes_nothing_and_prevents_tool_dispatch_or_acceptance()
{
    let fixture = run_fixture();
    let bus = crate::stream::StreamBus::new(16);
    let mut subscription = bus.subscribe();
    fixture.controller.attach_stream_bus(bus).unwrap();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let tool = Arc::new(CountingTool::default());
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    fixture.controller.lock().unwrap().fail_at = Some(TestFailure::StreamObservation);
    let failure = prepared
        .run()
        .await
        .err()
        .expect("lost storage acknowledgement fences the controller");
    assert!(failure.to_string().contains("recovery required"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    assert!(subscription.try_recv().is_err());
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(snapshot.contract().invocations().is_empty());
    assert!(snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .content
        .activation_stream(&snapshot, &fixture.activation)
        .unwrap()
        .is_empty());
    let view = fixture
        .controller
        .lock()
        .unwrap()
        .content
        .project(&snapshot)
        .unwrap();
    assert!(view
        .activations
        .iter()
        .all(|activation| !activation.currently_accepted));
    assert!(view
        .activations
        .iter()
        .flat_map(|activation| &activation.partial_outputs)
        .all(|output| output.kind == OutputKind::Partial));
    assert_eq!(
        fixture
            .controller
            .activation_provider_usage(&fixture.activation)
            .unwrap()
            .calls,
        1
    );
    drop(provider);
    let Fixture {
        controller,
        ownership,
        owner,
        activation,
        _root,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let controller =
        SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    let reopened = controller.snapshot().unwrap();
    assert!(reopened
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert!(reopened.contract().invocations().is_empty());
    assert!(controller
        .lock()
        .unwrap()
        .content
        .activation_stream(&reopened, &activation)
        .unwrap()
        .is_empty());
    assert_eq!(
        controller
            .activation_provider_usage(&activation)
            .unwrap()
            .calls,
        1
    );
}

#[tokio::test]
async fn stream_boundary_refuses_child_smearing_and_stale_producer_without_publication() {
    use axocoatl_actor::{AgentStreamChunk, AgentStreamObserver};
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider, Arc::new(CountingTool::default())),
        )
        .unwrap();
    let observer = crate::session_dispatch::stream::ActivationStreamObserver::new(
        fixture.controller.clone(),
        fixture.activation.clone(),
    );
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(observer
        .observe(&AgentStreamChunk::ToolCallResult {
            source_agent: Some("child".into()),
            id: "call".into(),
            name: "tool".into(),
            result: serde_json::json!({}),
            is_error: false
        })
        .is_err());
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .content
        .activation_stream(&snapshot, &fixture.activation)
        .unwrap()
        .is_empty());
    prepared.run().await.unwrap();
    let snapshot = fixture.controller.snapshot().unwrap();
    let before = fixture
        .controller
        .lock()
        .unwrap()
        .content
        .activation_stream(&snapshot, &fixture.activation)
        .unwrap();
    assert!(observer
        .observe(&AgentStreamChunk::Text("late stale text".into()))
        .is_err());
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(
        fixture
            .controller
            .lock()
            .unwrap()
            .content
            .activation_stream(&snapshot, &fixture.activation)
            .unwrap(),
        before
    );
}
