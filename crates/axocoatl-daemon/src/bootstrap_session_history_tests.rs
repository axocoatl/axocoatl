#![cfg(unix)]

use super::*;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use axocoatl_session::turn_contract::SessionId;

fn turn(store: &mut SessionTurnStore, session: &str, id: &str) {
    store
        .begin(BeginSessionTurn {
            turn_id: Some(id.into()),
            session_id: session.into(),
            user_input: format!("request {id}"),
            agent_id: Some("agent".into()),
            model: None,
            context: vec![],
            idempotency_key: None,
            metadata: Default::default(),
        })
        .unwrap();
    store
        .transition(
            id,
            format!("finish-{id}"),
            TransitionSessionTurn {
                status: SessionTurnLifecycle::Completed,
                final_output: Some(format!("answer {id}")),
                error: None,
                metadata: Default::default(),
            },
        )
        .unwrap();
}

#[test]
fn legacy_lifecycle_selection_preserves_rewind_delete_and_other_session_rows() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    turn(&mut legacy, "session-a", "a-first");
    turn(&mut legacy, "session-b", "b-first");
    turn(&mut legacy, "session-a", "a-second");
    let ownership =
        DataRootFormatOwnership::Legacy(LegacyFormatOwnership::acquire(root.path()).unwrap());
    let history = select_history(&ownership, None, &legacy, "session-a").unwrap();
    assert_eq!(
        history.legacy_rows(HistoryVisibility::Visible).unwrap(),
        legacy.list("session-a")
    );
    assert_eq!(
        history.legacy_transcript().unwrap(),
        legacy.transcript("session-a")
    );
    let expected = legacy.turns_through("session-a", Some("a-first")).unwrap();
    require_legacy_history_write(&ownership, false, HistoryMutation::Rewind).unwrap();
    assert_eq!(
        legacy
            .rewind("session-a", Some("a-first"), "rewind")
            .unwrap(),
        expected
    );
    assert_eq!(legacy.list_including_superseded("session-a").len(), 2);
    let foreign = legacy.list("session-b");
    for operation in [
        HistoryMutation::KeepAttempt,
        HistoryMutation::ReplaceEnvironment,
    ] {
        require_legacy_history_write(&ownership, false, operation).unwrap();
    }
    require_legacy_history_write(&ownership, false, HistoryMutation::DeleteSession).unwrap();
    assert_eq!(legacy.delete_session("session-a").unwrap(), 2);
    assert_eq!(legacy.list("session-b"), foreign);
    drop(legacy);
    let reopened = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    assert!(reopened.list_including_superseded("session-a").is_empty());
    assert_eq!(reopened.list("session-b"), foreign);
}

#[test]
fn upgraded_history_uses_exact_seal_and_refuses_all_lifecycle_effects() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    turn(&mut legacy, "session-a", "sealed");
    let upgraded = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let mut canonical = SessionExecutionStore::open(
        upgraded.clone(),
        ExecutionStoreOwner {
            workspace_id: "workspace-a".into(),
            session_id: SessionId::new("session-a").unwrap(),
        },
    )
    .unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let frontier = canonical.legacy_history_snapshot().unwrap();
    let retained = content.retain_legacy_history(&frontier).unwrap();
    canonical.seal_legacy_history(&retained).unwrap();
    // A mutable old ledger can differ after the seal. It must never override
    // the selected upgraded source or make an absent controller look legacy.
    turn(&mut legacy, "session-a", "outside-seal");
    let ownership = DataRootFormatOwnership::Upgraded(upgraded);
    let history = SessionHistory::from_upgraded(&canonical, &content).unwrap();
    let selected = select_history(&ownership, Some(history.clone()), &legacy, "session-a").unwrap();
    assert_eq!(
        selected
            .entries(HistoryVisibility::Visible)
            .iter()
            .map(|entry| entry.turn_id())
            .collect::<Vec<_>>(),
        ["sealed"]
    );
    assert!(select_history(&ownership, None, &legacy, "session-a").is_err());
    assert!(select_history(&ownership, Some(history), &legacy, "session-b").is_err());
    let before = std::fs::read(legacy.path()).unwrap();
    let canonical_before = std::fs::read(canonical.path()).unwrap();
    for operation in [
        HistoryMutation::Rewind,
        HistoryMutation::ExploreAttempts,
        HistoryMutation::KeepAttempt,
        HistoryMutation::DiscardAttempts,
        HistoryMutation::ReplaceEnvironment,
        HistoryMutation::DeleteSession,
    ] {
        let mut effect_entered = false;
        let outcome: Result<(), DaemonError> = (|| {
            require_legacy_history_write(&ownership, false, operation)?;
            effect_entered = true;
            legacy
                .delete_session("session-a")
                .map_err(|error| DaemonError::Session(error.to_string()))?;
            Ok(())
        })();
        assert!(outcome.is_err());
        assert!(!effect_entered);
    }
    assert_eq!(std::fs::read(legacy.path()).unwrap(), before);
    assert_eq!(std::fs::read(canonical.path()).unwrap(), canonical_before);
}

#[test]
fn retained_canonical_controller_never_grants_legacy_mutation() {
    let root = tempfile::tempdir().unwrap();
    let ownership =
        DataRootFormatOwnership::Legacy(LegacyFormatOwnership::acquire(root.path()).unwrap());
    for operation in [
        HistoryMutation::Rewind,
        HistoryMutation::ExploreAttempts,
        HistoryMutation::KeepAttempt,
        HistoryMutation::DiscardAttempts,
        HistoryMutation::ReplaceEnvironment,
        HistoryMutation::DeleteSession,
    ] {
        assert!(require_legacy_history_write(&ownership, true, operation).is_err());
    }
}
