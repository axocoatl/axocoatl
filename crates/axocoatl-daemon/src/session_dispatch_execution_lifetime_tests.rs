use super::*;

#[tokio::test]
async fn lifecycle_idle_observers_do_not_hold_execution_ownership() {
    let fixture = run_fixture();
    let observer = fixture.controller.clone();
    fixture
        .controller
        .close_registered_repository_admission()
        .unwrap();
    fixture
        .controller
        .wait_for_registered_executions(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        observer.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(observer
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true)),
                Arc::new(CountingTool::default())
            ),
        )
        .is_err());
}

#[tokio::test]
async fn lifecycle_prepared_cancellation_releases_without_dispatch() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Answer, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap();
    fixture
        .controller
        .close_registered_repository_admission()
        .unwrap();
    assert!(fixture
        .controller
        .wait_for_registered_executions(Duration::from_millis(10))
        .await
        .is_err());
    // A previously prepared handle cannot begin behavior after the fence.
    assert!(prepared.run().await.is_err());
    fixture
        .controller
        .wait_for_registered_executions(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lifecycle_direct_run_wait_retains_claimed_tool_after_cancelled_cleanup_wait() {
    let fixture = run_fixture();
    let observer = fixture.controller.clone();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let release = Arc::new(Notify::new());
    let tool = Arc::new(CountingTool {
        release: Some(release.clone()),
        ..Default::default()
    });
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(5), tool.started.notified())
        .await
        .unwrap();
    fixture
        .controller
        .close_registered_repository_admission()
        .unwrap();
    {
        let waiting = observer.wait_for_registered_executions(Duration::from_secs(5));
        tokio::pin!(waiting);
        tokio::select! {
            biased;
            result = &mut waiting => panic!("live tool falsely released cleanup: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        // Dropping the cleanup wait must not drop the separately owned tool.
    }
    assert!(observer
        .wait_for_registered_executions(Duration::from_millis(10))
        .await
        .is_err());
    release.notify_one();
    let settled = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!settled.accepted);
    observer
        .wait_for_registered_executions(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let state = observer.lock().unwrap();
    assert!(
        !state.bound.is_empty(),
        "historical bindings do not keep execution live"
    );
    assert_eq!(
        state
            .canonical
            .snapshot(&state.turn_id)
            .unwrap()
            .contract()
            .invocations()[0]
            .evidence
            .disposition(),
        EffectDisposition::OutcomeRecorded
    );
}

#[tokio::test]
async fn lifecycle_aborted_direct_run_keeps_tool_owned_until_real_outcome_is_recorded() {
    let fixture = run_fixture();
    let observer = fixture.controller.clone();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Tools, true));
    let release = Arc::new(Notify::new());
    let tool = Arc::new(CountingTool {
        release: Some(release.clone()),
        ..Default::default()
    });
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(&fixture, provider.clone(), tool.clone()),
        )
        .unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(5), tool.started.notified())
        .await
        .unwrap();
    run.abort();
    assert!(run.await.err().expect("aborted direct run").is_cancelled());
    observer.close_registered_repository_admission().unwrap();
    // Neither the activation future nor its scheduling JoinSet exists now.
    // The separately owned invocation must still retain the actual backend.
    assert!(observer
        .wait_for_registered_executions(Duration::from_millis(20))
        .await
        .is_err());
    assert_eq!(
        observer.snapshot().unwrap().contract().invocations()[0]
            .evidence
            .disposition(),
        EffectDisposition::OutcomeUnknown
    );
    release.notify_one();
    observer
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    let state = observer.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    let invocation = &snapshot.contract().invocations()[0];
    assert_eq!(
        invocation.evidence.disposition(),
        EffectDisposition::OutcomeRecorded
    );
    assert!(state
        .audit
        .invocation(&invocation.invocation_id)
        .unwrap()
        .unwrap()
        .final_evidence
        .is_some());
    assert!(snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lifecycle_aborted_direct_provider_retains_unknown_usage_before_idle() {
    let fixture = run_fixture();
    let provider = Arc::new(RunProvider::new(&fixture, RunProviderMode::Pending, true));
    let prepared = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            resources(
                &fixture,
                provider.clone(),
                Arc::new(CountingTool::default()),
            ),
        )
        .unwrap();
    let run = tokio::spawn(prepared.run());
    tokio::time::timeout(Duration::from_secs(5), provider.started.notified())
        .await
        .unwrap();
    run.abort();
    assert!(run.await.err().expect("aborted direct run").is_cancelled());
    fixture
        .controller
        .close_registered_repository_admission()
        .unwrap();
    fixture
        .controller
        .wait_for_registered_executions(Duration::from_secs(1))
        .await
        .unwrap();
    let usage = fixture
        .controller
        .activation_provider_usage(&fixture.activation)
        .unwrap();
    assert_eq!(usage.calls, 1);
    assert!(!usage.tokens.complete);
}

#[tokio::test]
async fn lifecycle_factory_wait_is_cancelled_without_provider_dispatch() {
    let fixture = input_fixture();
    let provider = InputProvider::new("must-not-start", false, false);
    let mut pending = driver_plan(&fixture.parent, provider.clone());
    pending.wait = Some(Arc::new(Notify::new()));
    let factory = DriverFactory::new(vec![pending]);
    let driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory.clone())
        .unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(5), factory.started.notified())
        .await
        .unwrap();
    fixture
        .controller
        .close_registered_repository_admission()
        .unwrap();
    fixture
        .controller
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    let _outcome = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.resolved.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn lifecycle_driver_drop_keeps_detached_child_ownership_until_tool_settles() {
    let fixture = input_fixture();
    let observer = fixture.controller.clone();
    let provider = InputProvider::new("must-not-accept", true, false);
    let release = Arc::new(Notify::new());
    let tool = Arc::new(CountingTool {
        release: Some(release.clone()),
        ..Default::default()
    });
    let mut plan = driver_plan(&fixture.parent, provider.clone());
    plan.tool = Some(tool.clone());
    let factory = DriverFactory::new(vec![plan]);
    let driver = fixture
        .controller
        .autonomous_turn_driver(driver_seeds(&fixture), factory)
        .unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(5), tool.started.notified())
        .await
        .unwrap();
    run.abort();
    assert!(run.await.err().expect("aborted driver").is_cancelled());
    assert!(observer.lock().unwrap().driver.is_none());
    observer.close_registered_repository_admission().unwrap();
    assert!(observer
        .wait_for_registered_executions(Duration::from_millis(10))
        .await
        .is_err());
    release.notify_one();
    observer
        .wait_for_registered_executions(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let snapshot = observer.snapshot().unwrap();
    assert!(snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert_eq!(
        snapshot.contract().invocations()[0].evidence.disposition(),
        EffectDisposition::OutcomeRecorded
    );
}
