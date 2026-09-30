use super::*;
use crate::bootstrap::session_dispatch::{
    PendingSessionToken, RegisteredControlPlane, SessionDispatchRegistry,
};
use crate::session_dispatch::{RetainedSessionStores, SuccessorTurn};
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ExecutionContentStore, ExecutionRequestContent,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::session_history::HistoryVisibility;
use axocoatl_session::turn_contract::*;

fn held_stores(canonical: SessionExecutionStore) -> RetainedSessionStores {
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap();
    RetainedSessionStores {
        canonical,
        content,
        memory,
    }
}

fn first_spec(registry: &SessionDispatchRegistry, token: &PendingSessionToken) -> SuccessorTurn {
    registry
        .prepare_first_turn_content(token, |_, content, _, _| {
            let definition_id = AgentDefinitionId::new("first-definition").unwrap();
            let definition = content
                .retain_activation_evidence(ActivationEvidenceContent::Definition {
                    definition_id: definition_id.clone(),
                    revision: 1,
                    profile: axocoatl_session::control_authority::ExecutionProfile {
                        definition: definition_id.as_str().into(),
                        provider: "local".into(),
                        model: "model".into(),
                        isolation: "in-process".into(),
                        tools: vec![],
                    },
                    configuration: "{}".into(),
                })
                .unwrap();
            let turn_id = LogicalTurnId::new("first-native-turn").unwrap();
            Ok(SuccessorTurn {
                command_id: CommandId::new("first-native-begin").unwrap(),
                turn_id: turn_id.clone(),
                epoch_id: ExecutionEpochId::new("first-native-epoch").unwrap(),
                graph: TurnGraphSnapshot {
                    snapshot_id: GraphSnapshotId::new("first-native-graph").unwrap(),
                    revision: 1,
                    nodes: vec![GraphNode {
                        node_id: TurnNodeId::new("node").unwrap(),
                        slot_id: SessionTeamSlotId::new("slot").unwrap(),
                        definition: DefinitionSnapshotRef {
                            definition_id,
                            snapshot: definition.reference().clone(),
                        },
                        conversation_id: NodeConversationId::new("conversation").unwrap(),
                        starting_savepoint: ConversationSavepoint::Empty,
                        required: true,
                    }],
                    dependencies: vec![],
                    conditions: vec![],
                },
                request: ExecutionRequestContent {
                    turn_id,
                    recorded_at_unix_ms: 1,
                    display_input: "Actual first request".into(),
                    effective_input: "Actual first request".into(),
                    context: vec![],
                    target_definition: None,
                    model: None,
                },
            })
        })
        .unwrap()
}

#[tokio::test]
async fn native_empty_session_close_reopen_preserves_explicit_origin_and_rejects_stale_token() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let (session, receipt) = sessions
        .create_native_with_environment(
            &ownership,
            "Native",
            "workspace",
            workspace.path(),
            SessionMode::SingleAgent {
                agent_id: "agent".into(),
            },
            vec![],
            vec![],
            None,
            None,
            false,
            true,
        )
        .unwrap();
    let owner = receipt.owner().clone();
    let registry = SessionDispatchRegistry::default();
    let token = registry
        .retain_native_session(ownership.clone(), receipt)
        .unwrap();
    let identity = registry.pending_identity(&token, &data).unwrap();
    assert!(registry.retains_session(&session.id).unwrap());
    assert!(registry
        .history_snapshot(&session.id)
        .unwrap()
        .unwrap()
        .entries(HistoryVisibility::IncludingSuperseded)
        .is_empty());
    assert!(matches!(
        registry
            .lookup_control_plane(&session.id, "missing café")
            .unwrap(),
        RegisteredControlPlane::MissingTurn
    ));
    assert!(!root.path().join("session-history").exists());
    // Read-only empty history never creates a legacy ledger to obtain a seal.
    let first = historical_read_tree(root.path());
    registry.history_snapshot(&session.id).unwrap();
    assert_eq!(historical_read_tree(root.path()), first);
    let cleanup = registry
        .prepare_session_cleanup(&session.id, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(registry.prepare_first_turn(&session.id).is_err());
    assert!(registry.require_session_reopenable(&session.id).is_err());
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    registry.reopen_session(&session.id).unwrap();
    // The stale token is deliberately still alive; completed cleanup releases
    // its original child locks so actual canonical reopen can succeed.
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    assert_eq!(canonical.identity().unwrap(), identity);
    assert!(canonical.native_origin().unwrap().is_some());
    assert!(canonical.legacy_seal().unwrap().is_none());
    let mut stores = Some(held_stores(canonical));
    let fresh = registry.retain_existing_session(&mut stores).unwrap();
    assert!(stores.is_none());
    assert!(registry.pending_identity(&token, &data).is_err());
    assert_eq!(registry.pending_identity(&fresh, &data).unwrap(), identity);
    registry.close_all_admission().unwrap();
    assert!(registry.prepare_first_turn(&session.id).is_err());
    let cleanup = registry
        .prepare_session_cleanup(&session.id, Duration::from_secs(1))
        .await
        .unwrap();
    registry.complete_session_cleanup(&cleanup).unwrap();
    registry.forget_deleted_session(&session.id).unwrap();
}

#[tokio::test]
async fn first_begin_moves_exact_retained_stores_and_preserves_sealed_legacy_history() {
    let mut f = fixture_with_legacy_turn(Some("turn café")).await;
    let session_id = f.owner.metadata().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let canonical = f._canonical.take().unwrap();
    let original_identity = canonical.identity().unwrap();
    let seal = canonical.legacy_seal().unwrap().unwrap();
    let stores = held_stores(canonical);
    let mut migrated = Some(crate::bootstrap::session_migration::MigratedSessionState {
        canonical: stores.canonical,
        content: stores.content,
        activation_state: stores.memory,
        seal,
        assignments: vec![],
    });
    let token = registry.retain_migrated_session(&mut migrated).unwrap();
    assert!(migrated.is_none());
    let history = registry.history_snapshot(&session_id).unwrap().unwrap();
    assert_eq!(
        history
            .entries(HistoryVisibility::IncludingSuperseded)
            .len(),
        1
    );
    assert!(matches!(
        registry
            .lookup_control_plane(&session_id, "turn café")
            .unwrap(),
        RegisteredControlPlane::Found(_)
    ));
    let spec = first_spec(&registry, &token);
    let (controller, _) = registry
        .begin_first_turn(&token, f.owner.clone(), spec)
        .unwrap();
    assert_eq!(
        controller.snapshot().unwrap().journal_id(),
        original_identity.journal_id()
    );
    assert_eq!(
        registry
            .history_snapshot(&session_id)
            .unwrap()
            .unwrap()
            .entries(HistoryVisibility::IncludingSuperseded)
            .len(),
        2
    );
    assert!(registry
        .pending_identity(&token, &f.owner.inner.data_root)
        .is_err());
    assert!(f.operation.try_lock().is_err());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.owner.sandbox().list_terminals().len(), 1);
    closed_successor(&controller);
    registry
        .release_after_turn(&session_id, controller.snapshot().unwrap().turn_id())
        .unwrap();
    assert!(f.operation.try_lock().is_ok());
}

#[tokio::test]
async fn first_attachment_failure_keeps_all_namespaces_and_exact_begin_retry() {
    let mut f = fixture_with_legacy_turn(Some("old-turn")).await;
    let session_id = f.owner.metadata().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let mut held = Some(held_stores(f._canonical.take().unwrap()));
    let token = registry.retain_existing_session(&mut held).unwrap();
    let identity = registry
        .pending_identity(&token, &f.owner.inner.data_root)
        .unwrap();
    let spec = first_spec(&registry, &token);
    // Compete for the real child lock, not a fake constructor-error switch.
    let audit = registry
        .prepare_first_turn_content(&token, |canonical, _, _, _| {
            Ok(
                axocoatl_session::invocation_audit::InvocationAudit::open_owned(
                    canonical
                        .component_namespace(ExecutionComponent::InvocationAudit)
                        .unwrap(),
                )
                .unwrap(),
            )
        })
        .unwrap();
    assert!(registry
        .begin_first_turn(&token, f.owner.clone(), spec)
        .is_err());
    assert_eq!(
        registry
            .pending_identity(&token, &f.owner.inner.data_root)
            .unwrap(),
        identity
    );
    assert_eq!(
        registry
            .history_snapshot(&session_id)
            .unwrap()
            .unwrap()
            .entries(HistoryVisibility::IncludingSuperseded)
            .len(),
        2
    );
    let (journal_path, journal) = registry
        .prepare_first_turn_content(&token, |canonical, _, _, _| {
            assert!(canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .is_err());
            assert!(canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .is_err());
            Ok((canonical.path(), std::fs::read(canonical.path()).unwrap()))
        })
        .unwrap();
    assert!(f.operation.try_lock().is_err());
    drop(audit);
    let retry = first_spec(&registry, &token);
    let (controller, _) = registry
        .begin_first_turn(&token, f.owner.clone(), retry)
        .unwrap();
    assert_eq!(
        controller.snapshot().unwrap().journal_id(),
        identity.journal_id()
    );
    assert_eq!(std::fs::read(journal_path).unwrap(), journal);
    assert_eq!(controller.snapshot().unwrap().contract().revision(), 1);
}

#[tokio::test]
async fn failed_first_begin_cleanup_parks_workspace_and_refuses_unknown_work() {
    let mut f = fixture_with_legacy_turn(Some("old-turn")).await;
    let session_id = f.owner.metadata().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let mut held = Some(held_stores(f._canonical.take().unwrap()));
    let token = registry.retain_existing_session(&mut held).unwrap();
    let mut bad = first_spec(&registry, &token);
    bad.request.turn_id = LogicalTurnId::new("foreign-turn").unwrap();
    assert!(registry
        .begin_first_turn(&token, f.owner.clone(), bad)
        .is_err());
    assert!(f.operation.try_lock().is_err());
    let mut cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    let operation = cleanup.take_operation().unwrap();
    drop(operation);
    drop(cleanup); // Models caller cancellation after ownership transferred.
    assert!(f.operation.try_lock().is_err());
    let cleanup = registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .unwrap();
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    assert!(f.operation.try_lock().is_ok());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);

    let mut f = fixture_with_legacy_turn(Some("another-old-turn")).await;
    let session_id = f.owner.metadata().session_id.clone();
    let registry = SessionDispatchRegistry::default();
    let mut held = Some(held_stores(f._canonical.take().unwrap()));
    let token = registry.retain_existing_session(&mut held).unwrap();
    let mut bad = first_spec(&registry, &token);
    bad.request.turn_id = LogicalTurnId::new("foreign-turn").unwrap();
    assert!(registry
        .begin_first_turn(&token, f.owner.clone(), bad)
        .is_err());
    let id = arm_and_drop(&f.owner).await;
    assert!(registry
        .prepare_session_cleanup(&session_id, Duration::from_secs(1))
        .await
        .is_err());
    assert_held(&f, &id);
    assert!(registry.retains_session(&session_id).unwrap());
}

#[tokio::test]
async fn unclassified_empty_existing_session_is_retained_but_never_inferred_native() {
    let mut f = fixture().await;
    let registry = SessionDispatchRegistry::default();
    let session_id = f.owner.metadata().session_id.clone();
    let canonical = f._canonical.take().unwrap();
    assert!(canonical.native_origin().unwrap().is_none());
    let mut source = Some(held_stores(canonical));
    assert!(registry.retain_existing_session(&mut source).is_err());
    assert!(source.is_none());
    assert!(registry.retains_session(&session_id).unwrap());
    assert!(registry.history_snapshot(&session_id).is_err());
    assert!(registry
        .lookup_control_plane(&session_id, "missing")
        .is_err());
    assert!(!f._data.path().join("session-history").exists());
}

#[tokio::test]
async fn actual_empty_migration_returned_stores_join_registry_without_reopening_children() {
    use crate::bootstrap::session_migration::{migrate_held_session_state, LegacySessionMigration};
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let legacy =
        axocoatl_session::SessionTurnStore::open_in_secure(&data, "session-history").unwrap();
    let actual_empty_ledger = std::fs::read(legacy.path()).unwrap();
    drop(legacy);
    let checkpoints = axocoatl_memory::CheckpointStore::new_in_secure(
        &data,
        "checkpoints",
        axocoatl_memory::CheckpointPolicy::Manual,
    )
    .unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let actors = axocoatl_actor::AgentRegistry::new();
    let active = AsyncMutex::new(HashMap::new());
    let mut converted = migrate_held_session_state(
        ownership,
        &data,
        &checkpoints,
        &actors,
        &active,
        &[LegacySessionMigration {
            session_id: "empty-migrated-session".into(),
            workspace_id: "workspace".into(),
            actors: vec![],
        }],
        &str::len,
    )
    .await
    .unwrap();
    let identity = converted[0].canonical.identity().unwrap();
    let registry = SessionDispatchRegistry::default();
    let mut source = Some(converted.remove(0));
    let token = registry.retain_migrated_session(&mut source).unwrap();
    assert!(source.is_none());
    assert_eq!(registry.pending_identity(&token, &data).unwrap(), identity);
    assert!(registry
        .history_snapshot("empty-migrated-session")
        .unwrap()
        .unwrap()
        .entries(HistoryVisibility::IncludingSuperseded)
        .is_empty());
    assert_eq!(
        std::fs::read(root.path().join("session-history/turns.v1.jsonl")).unwrap(),
        actual_empty_ledger
    );
    registry
        .prepare_first_turn_content(&token, |canonical, _, _, _| {
            assert!(canonical.native_origin().unwrap().is_none());
            assert!(canonical.legacy_seal().unwrap().is_some());
            assert!(canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .is_err());
            assert!(canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .is_err());
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn partial_native_initialization_retains_canonical_until_explicit_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let (session, receipt) = sessions
        .create_native_with_environment(
            &ownership,
            "Native",
            "workspace",
            workspace.path(),
            SessionMode::SingleAgent {
                agent_id: "agent".into(),
            },
            vec![],
            vec![],
            None,
            None,
            false,
            true,
        )
        .unwrap();
    let owner = receipt.owner().clone();
    let canonical = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let identity = canonical.identity().unwrap();
    // Every child namespace retains the canonical inode lock. Release the
    // actual content owner first, then inject a bounded malformed journal so
    // canonical reopen succeeds and content initialization itself fails.
    drop(content);
    let namespace = canonical
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    let original_content = namespace
        .read_limited("execution-content.v1.json", 64 * 1024 * 1024)
        .unwrap();
    namespace
        .atomic_write("execution-content.v1.json", b"{")
        .unwrap();
    drop(namespace);
    drop(canonical);
    let registry = SessionDispatchRegistry::default();
    let failure = match registry.retain_native_session(ownership.clone(), receipt) {
        Ok(_) => panic!("malformed content journal was accepted"),
        Err(failure) => failure,
    };
    assert!(failure.to_string().contains("EOF"), "{failure}");
    assert!(registry.retains_session(&session.id).unwrap());
    assert!(registry.history_snapshot(&session.id).is_err());
    // A failed content attachment did not release the canonical inode lock.
    assert!(SessionExecutionStore::open(ownership.clone(), owner.clone()).is_err());
    let token = registry.prepare_first_turn(&session.id).unwrap();
    assert!(registry.pending_identity(&token, &data).is_err());
    let cleanup = registry
        .prepare_session_cleanup(&session.id, Duration::from_secs(1))
        .await
        .unwrap();
    registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    registry.reopen_session(&session.id).unwrap();
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    assert_eq!(canonical.identity().unwrap(), identity);
    assert!(canonical.native_origin().unwrap().is_some());
    let namespace = canonical
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    namespace
        .atomic_write("execution-content.v1.json", &original_content)
        .unwrap();
    drop(namespace);
    let mut stores = Some(held_stores(canonical));
    let fresh = registry.retain_existing_session(&mut stores).unwrap();
    assert_eq!(registry.pending_identity(&fresh, &data).unwrap(), identity);
    assert!(registry.pending_identity(&token, &data).is_err());
}

#[tokio::test]
async fn first_begin_refuses_visible_and_hidden_sealed_identity_before_any_request_or_begin_write()
{
    for hidden in [false, true] {
        let mut fixture =
            fixture_with_legacy_turn_visibility(Some("first-native-turn"), hidden).await;
        let session_id = fixture.owner.metadata().session_id.clone();
        let registry = SessionDispatchRegistry::default();
        let mut held = Some(held_stores(fixture._canonical.take().unwrap()));
        let token = registry.retain_existing_session(&mut held).unwrap();
        let spec = first_spec(&registry, &token);
        let before = historical_read_tree(fixture._data.path());
        let failure = registry
            .begin_first_turn(&token, fixture.owner.clone(), spec)
            .err()
            .expect("collision must refuse");
        assert!(failure
            .to_string()
            .contains("occupied by retained legacy history"));
        assert_eq!(historical_read_tree(fixture._data.path()), before);
        assert!(
            registry.prepare_first_turn(&session_id).is_ok(),
            "refused admission preserves the pending owner"
        );
        let history = registry.history_snapshot(&session_id).unwrap().unwrap();
        assert_eq!(
            history
                .entries(HistoryVisibility::IncludingSuperseded)
                .len(),
            1
        );
        assert_eq!(
            history.get("first-native-turn").unwrap().is_visible(),
            !hidden
        );
        assert_eq!(fixture.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    }
}
