//! Real prepared native activation settlement used by the existing Ways tasks.
//! The finite fixture provider records synthetic usage; no external-model claim.
use super::*;

#[tokio::test]
async fn completed_way_wrapper_preserves_accepted_output_and_cleanup_ownership() {
    let mut fixture = fixture().await;
    let run = run(&mut fixture, &[], true);
    let prepared = run
        .controller
        .prepare_repository_activation(
            run.activation.clone(),
            run.resources(Provider::new(vec![])),
            run.resource.clone(),
        )
        .unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let outcome = crate::bootstrap::native_ways::NativeWayExecution::new(
        prepared,
        run.controller.clone(),
        run.activation.clone(),
    )
    .run(trace)
    .await
    .unwrap_or_else(|failure| panic!("Way execution failed: {}", failure.error));
    let axocoatl_actor::AgentRunOutcome::Completed(output) = outcome.outcome else {
        panic!("actual accepted candidate must remain completed");
    };
    assert_eq!(output.content, "repository operation complete");
    assert!(outcome.token_usage.complete);
    assert_eq!(
        run.controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert!(
        fixture.operation.try_lock().is_err(),
        "Keep/no-Keep still owns isolated candidate cleanup"
    );
}

#[tokio::test]
async fn native_way_task_closes_only_after_actual_output_and_keeps_exact_usage() {
    let mut fixture = fixture().await;
    let run = run(&mut fixture, &[], true);
    let provider = Provider::new(vec![]);
    let prepared = run
        .controller
        .prepare_repository_activation(
            run.activation.clone(),
            run.resources(provider.clone()),
            run.resource.clone(),
        )
        .unwrap();
    assert_eq!(
        run.controller.live_owned_turn().unwrap(),
        Some(run.activation.turn_id.clone())
    );
    let settled = prepared.run().await.unwrap();
    assert!(settled.accepted);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        run.controller
            .activation_provider_usage(&run.activation)
            .unwrap()
            .tokens
            .usage
            .input_tokens,
        10
    );
    assert!(run
        .controller
        .native_way_route(&run.activation)
        .unwrap()
        .is_empty());
    assert!(run
        .controller
        .settle_native_way_task(&run.activation, None)
        .unwrap());
    let snapshot = run.controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::Completed)
    );
    assert_eq!(
        snapshot.contract().current_accepted_activations()[0]
            .output
            .as_ref(),
        Some(settled.output.reference())
    );
    run.registry
        .release_after_turn(run.activation.session_id.as_str(), &run.activation.turn_id)
        .unwrap();
    assert!(run.controller.live_owned_turn().unwrap().is_none());
    assert!(fixture.operation.try_lock().is_ok());
    let records = snapshot.contract().revision();
    assert!(run
        .controller
        .settle_native_way_task(&run.activation, None)
        .unwrap());
    assert_eq!(
        run.controller.snapshot().unwrap().contract().revision(),
        records
    );
}

#[tokio::test]
async fn native_way_stop_keeps_partial_generation_and_never_calls_provider() {
    let mut fixture = fixture().await;
    let run = run(&mut fixture, &[], true);
    let provider = Provider::new(vec![]);
    let prepared = run
        .controller
        .prepare_repository_activation(
            run.activation.clone(),
            run.resources(provider.clone()),
            run.resource.clone(),
        )
        .unwrap();
    run.controller
        .request_human_turn_stop(
            run.activation.session_id.as_str(),
            run.activation.turn_id.as_str(),
        )
        .unwrap();
    assert!(prepared.run().await.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(run
        .controller
        .settle_native_way_task(&run.activation, Some("Stopped"))
        .unwrap());
    let snapshot = run.controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::Cancelled)
    );
    assert!(snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    run.registry
        .release_after_turn(run.activation.session_id.as_str(), &run.activation.turn_id)
        .unwrap();
    assert!(fixture.operation.try_lock().is_ok());
}
