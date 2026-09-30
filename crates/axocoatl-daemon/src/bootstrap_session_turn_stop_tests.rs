use super::*;
use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
use axocoatl_session::turn_contract::LogicalTurnState;

#[tokio::test]
async fn registered_whole_stop_is_exact_idempotent_and_keeps_repository_open() {
    let mut f = fixture().await;
    let registry = SessionDispatchRegistry::default();
    let (controller, _) = begin_registered(&registry, &mut f);
    let snapshot = controller.snapshot().unwrap();
    let session_id = snapshot.owner().session_id.as_str();
    let turn_id = snapshot.turn_id().as_str();
    let before = historical_read_tree(f._data.path());
    assert!(registry
        .request_human_turn_stop(session_id, "foreign-turn")
        .is_err());
    assert!(registry
        .request_human_turn_stop("foreign-session", turn_id)
        .unwrap()
        .is_none());
    assert_eq!(historical_read_tree(f._data.path()), before);
    assert_eq!(
        registry
            .request_human_turn_stop(session_id, turn_id)
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        registry
            .request_human_turn_stop(session_id, turn_id)
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        controller.snapshot().unwrap().contract().state(),
        Some(LogicalTurnState::Cancelled)
    );
    assert!(f.operation.try_lock().is_err(), "Workspace remains held");
    drop(f.owner.execution_lease().await.unwrap());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    registry
        .release_after_turn(session_id, snapshot.turn_id())
        .unwrap();
    assert!(
        registry
            .request_human_turn_stop(session_id, turn_id)
            .is_err(),
        "retired entry cannot accept repeat control"
    );
}

#[tokio::test]
async fn registry_lifecycle_fence_refuses_stop_without_mutating_turn() {
    let mut f = fixture().await;
    let registry = SessionDispatchRegistry::default();
    let (controller, _) = begin_registered(&registry, &mut f);
    let snapshot = controller.snapshot().unwrap();
    registry.close_all_admission().unwrap();
    let before = controller.snapshot().unwrap().contract().revision();
    assert!(registry
        .request_human_turn_stop(
            snapshot.owner().session_id.as_str(),
            snapshot.turn_id().as_str()
        )
        .is_err());
    assert_eq!(controller.snapshot().unwrap().contract().revision(), before);
    assert!(controller
        .snapshot()
        .unwrap()
        .contract()
        .stop_requested()
        .is_none());
}
