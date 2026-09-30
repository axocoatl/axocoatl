use super::*;

#[tokio::test]
async fn whole_stop_fences_unmaterialized_child_and_factory_wait_without_retiring_driver_owner() {
    let fixture = input_fixture();
    let parent = InputProvider::new(PARENT_V1, true, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let wait = Arc::new(Notify::new());
    let mut parent_plan = driver_plan(&fixture.parent, parent.clone());
    parent_plan.wait = Some(wait.clone());
    let factory = DriverFactory::new(vec![parent_plan, driver_plan(&fixture.child, child.clone())]);
    let driver = fixture.controller.autonomous_turn_driver(driver_seeds(&fixture), factory.clone()).unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(3), factory.started.notified()).await.unwrap();
    let before = fixture.controller.snapshot().unwrap();
    let original_graph = before.contract().graph().unwrap().clone();
    assert_eq!(before.contract().activations().len(), 1);
    fixture.controller.request_human_turn_stop(before.owner().session_id.as_str(), before.turn_id().as_str()).unwrap();
    // No factory release is needed: Stop cancels that allocation wait. An old
    // result arriving later cannot bind either an actor or its dependent child.
    let outcome = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    wait.notify_one();
    assert_eq!(outcome.snapshot.contract().state(), Some(LogicalTurnState::Cancelled));
    assert_eq!(outcome.snapshot.contract().graph(), Some(&original_graph));
    assert_eq!(outcome.snapshot.contract().activations().len(), 1);
    assert!(outcome.snapshot.contract().stop_requested().unwrap().unrun_nodes.contains(&fixture.child.input.activation.node_id));
    assert!(outcome.finalized.unwrap().promotion().selected.is_empty());
    assert_eq!(parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
    let state = fixture.controller.lock().unwrap();
    assert!(!state.execution_admission_closed, "user Stop must not retire lifecycle ownership");
    assert!(state.driver.is_none());
    assert!(state.execution_lifetimes.is_idle());
}

#[tokio::test]
async fn whole_stop_preserves_accepted_parent_and_promotes_only_that_exact_generation() {
    let fixture = input_fixture();
    let parent = InputProvider::new(PARENT_V1, false, false);
    let child = InputProvider::new(CHILD_V1, false, false);
    let wait = Arc::new(Notify::new());
    let mut child_plan = driver_plan(&fixture.child, child.clone());
    child_plan.wait = Some(wait);
    let factory = DriverFactory::new(vec![driver_plan(&fixture.parent, parent.clone()), child_plan]);
    let driver = fixture.controller.autonomous_turn_driver(driver_seeds(&fixture), factory.clone()).unwrap();
    let run = tokio::spawn(driver.run());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let notification = factory.started.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if factory.resolved.lock().unwrap().len() == 2 { break; }
            notification.await;
        }
    }).await.unwrap();
    let before = fixture.controller.snapshot().unwrap();
    let parent_checkpoint = before.contract().current_accepted_activations()[0].checkpoint.clone().unwrap();
    fixture.controller.request_human_turn_stop(before.owner().session_id.as_str(), before.turn_id().as_str()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(3), run).await.unwrap().unwrap().unwrap();
    let finalized = outcome.finalized.unwrap();
    assert_eq!(finalized.promotion().selected.len(), 1);
    assert_eq!(finalized.promotion().selected[0].accepted, parent_checkpoint);
    assert_eq!(outcome.snapshot.contract().state(), Some(LogicalTurnState::Cancelled));
    assert_eq!(parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
}
