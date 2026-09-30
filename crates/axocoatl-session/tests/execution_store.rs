#![cfg(unix)]

use std::fs;
use std::sync::Arc;

use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::*;
use axocoatl_session::turn_contract::*;

fn owner() -> ExecutionStoreOwner {
    ExecutionStoreOwner {
        workspace_id: "client-workspace".into(),
        session_id: SessionId::new("session-a").unwrap(),
    }
}

fn boundary(root: &tempfile::TempDir) -> Arc<UpgradedFormatOwnership> {
    Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    )
}

#[test]
fn runtime_binding_requires_the_actual_held_data_root() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let store = SessionExecutionStore::open(boundary(&root), owner()).unwrap();
    let actual = axocoatl_core::SecureDir::open_existing_all(root.path()).unwrap();
    store.verify_data_root(&actual).unwrap();
    let wrong = axocoatl_core::SecureDir::open_existing_all(foreign.path()).unwrap();
    assert!(store.verify_data_root(&wrong).is_err());
    // The same path spelling after replacement cannot attach the old journal
    // to a new host root; both held capabilities must still name their inode.
    let moved = root.path().with_extension("held-root");
    fs::rename(root.path(), &moved).unwrap();
    fs::create_dir(root.path()).unwrap();
    let replacement = axocoatl_core::SecureDir::open_existing_all(root.path()).unwrap();
    assert!(store.verify_data_root(&replacement).is_err());
    drop(store);
    fs::remove_dir_all(moved).unwrap();
}

fn graph() -> TurnGraphSnapshot {
    TurnGraphSnapshot {
        snapshot_id: GraphSnapshotId::new("graph-v1").unwrap(),
        revision: 1,
        nodes: vec![GraphNode {
            node_id: TurnNodeId::new("tester").unwrap(),
            slot_id: SessionTeamSlotId::new("qa").unwrap(),
            definition: DefinitionSnapshotRef {
                definition_id: AgentDefinitionId::new("tester").unwrap(),
                snapshot: EvidenceRef::new("definition-v1").unwrap(),
            },
            conversation_id: NodeConversationId::new("qa-conversation").unwrap(),
            starting_savepoint: ConversationSavepoint::Empty,
            required: true,
        }],
        dependencies: vec![],
        conditions: vec![],
    }
}

fn event(turn: &str, revision: u64, id: &str, event: TurnContractEvent) -> TurnContractEnvelope {
    TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new(id).unwrap(),
        expected_revision: revision,
        session_id: owner().session_id,
        turn_id: LogicalTurnId::new(turn).unwrap(),
        event,
    }
}

fn begin(turn: &str, id: &str, predecessor: Option<ClosedTurnRef>) -> TurnContractEnvelope {
    event(
        turn,
        0,
        id,
        TurnContractEvent::Begin {
            epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
            graph: graph(),
            predecessor,
        },
    )
}

fn close(turn: &str, revision: u64, id: &str) -> TurnContractEnvelope {
    event(
        turn,
        revision,
        id,
        TurnContractEvent::Close {
            closure: TurnClosure::Finished,
        },
    )
}

#[test]
fn reopened_running_turn_becomes_attention_once_and_original_receipt_survives() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    let request = begin("turn-a", "begin-a", None);
    let receipt = store.append(request.clone()).unwrap();
    assert_eq!(receipt.turn_revision(), 1);
    drop(store);
    let mut recovered = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    let (id, turn) = recovered.unfinished_turn().unwrap().unwrap();
    assert_eq!(id.as_str(), "turn-a");
    assert_eq!(turn.state(), Some(LogicalTurnState::NeedsAttention));
    assert_eq!(turn.epochs()[0].state, EpochState::Interrupted);
    assert_eq!(turn.revision(), 2);
    assert_eq!(recovered.append(request).unwrap(), receipt);
    drop(recovered);
    let again = SessionExecutionStore::open(guard, owner()).unwrap();
    assert_eq!(again.records().unwrap().len(), 2);
}

#[test]
fn unfinished_ownership_includes_attention_and_closed_successor_resolves_exact_history() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    store.append(begin("turn-a", "begin-a", None)).unwrap();
    assert!(matches!(
        store.append(begin("turn-b", "begin-b", None)),
        Err(ExecutionStoreError::UnfinishedTurn)
    ));
    drop(store);
    let mut store = SessionExecutionStore::open(guard, owner()).unwrap();
    assert!(matches!(
        store.append(begin("turn-b", "begin-b", None)),
        Err(ExecutionStoreError::UnfinishedTurn)
    ));
    store.append(close("turn-a", 2, "close-a")).unwrap();
    let predecessor = store
        .turn(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap()
        .unwrap()
        .closed_reference()
        .unwrap();
    store
        .append(begin("turn-b", "begin-b", Some(predecessor.clone())))
        .unwrap();
    assert_eq!(
        store.unfinished_turn().unwrap().unwrap().1.predecessor(),
        Some(&predecessor)
    );
}

#[test]
fn closed_reference_from_another_projection_cannot_forge_canonical_history() {
    let root = tempfile::tempdir().unwrap();
    let mut store = SessionExecutionStore::open(boundary(&root), owner()).unwrap();
    let mut fabricated = TurnContract::default();
    fabricated
        .apply(&begin("absent", "other-begin", None))
        .unwrap();
    fabricated
        .apply(&close("absent", 1, "other-close"))
        .unwrap();
    let predecessor = fabricated.closed_reference().unwrap();
    let before = fs::read(store.path()).unwrap();
    assert!(store
        .append(begin("turn-a", "begin-a", Some(predecessor)))
        .is_err());
    assert_eq!(fs::read(store.path()).unwrap(), before);
    assert!(store.records().unwrap().is_empty());
}

#[test]
fn independent_sessions_share_format_guard_but_cannot_share_a_session_writer() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    let store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    assert!(SessionExecutionStore::open(guard.clone(), owner()).is_err());
    let mut other_owner = owner();
    other_owner.session_id = SessionId::new("session-b").unwrap();
    let other = SessionExecutionStore::open(guard.clone(), other_owner).unwrap();
    drop(guard);
    assert!(UpgradedFormatOwnership::open(root.path()).is_err());
    drop(store);
    assert!(UpgradedFormatOwnership::open(root.path()).is_err());
    drop(other);
    UpgradedFormatOwnership::open(root.path()).unwrap();
}

#[test]
fn command_identity_is_session_wide_and_changed_replay_cannot_mutate_closed_history() {
    let root = tempfile::tempdir().unwrap();
    let mut store = SessionExecutionStore::open(boundary(&root), owner()).unwrap();
    let first = begin("turn-a", "begin-a", None);
    let receipt = store.append(first.clone()).unwrap();
    store.append(close("turn-a", 1, "close-a")).unwrap();
    let before = fs::read(store.path()).unwrap();
    assert_eq!(store.append(first).unwrap(), receipt);
    assert!(matches!(
        store.append(begin("turn-b", "begin-a", None)),
        Err(ExecutionStoreError::Contract(
            TurnContractError::CommandConflict
        ))
    ));
    assert!(store.append(close("turn-a", 2, "late-close")).is_err());
    assert_eq!(fs::read(store.path()).unwrap(), before);
}

#[test]
fn foreign_workspace_and_corrupt_journal_fail_without_overwriting_evidence() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    store.append(begin("turn-a", "begin-a", None)).unwrap();
    let path = store.path();
    drop(store);
    let before = fs::read(&path).unwrap();
    let mut foreign = owner();
    foreign.workspace_id = "other-client".into();
    assert!(SessionExecutionStore::open(guard.clone(), foreign).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::write(&path, b"{truncated").unwrap();
    assert!(SessionExecutionStore::open(guard, owner()).is_err());
    assert_eq!(fs::read(path).unwrap(), b"{truncated");
}

#[test]
fn duplicate_or_foreign_canonical_records_fail_replay() {
    for mutation in ["duplicate", "owner", "schema"] {
        let root = tempfile::tempdir().unwrap();
        let guard = boundary(&root);
        let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
        store.append(begin("turn-a", "begin-a", None)).unwrap();
        let path = store.path();
        drop(store);
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match mutation {
            "duplicate" => {
                let duplicate = journal["records"][0].clone();
                journal["records"].as_array_mut().unwrap().push(duplicate);
            }
            "owner" => journal["ownership_id"] = "foreign-boundary".into(),
            _ => journal["schema_version"] = 3.into(),
        }
        let bytes = serde_json::to_vec(&journal).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(SessionExecutionStore::open(guard, owner()).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn failed_publication_poison_requires_reopen_before_any_receipt() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    let path = store.path();
    let saved = path.with_extension("saved");
    fs::rename(&path, &saved).unwrap();
    fs::create_dir(&path).unwrap();
    let request = begin("turn-a", "begin-a", None);
    assert!(store.append(request.clone()).is_err());
    fs::remove_dir(&path).unwrap();
    fs::rename(saved, path).unwrap();
    assert!(matches!(
        store.append(request.clone()),
        Err(ExecutionStoreError::RecoveryRequired)
    ));
    assert!(matches!(
        store.records(),
        Err(ExecutionStoreError::RecoveryRequired)
    ));
    drop(store);
    let mut reopened = SessionExecutionStore::open(guard, owner()).unwrap();
    assert!(reopened.records().unwrap().is_empty());
    reopened.append(request).unwrap();
}

#[test]
fn replaced_session_directory_and_changed_format_manifest_revoke_writing() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    let mut store = SessionExecutionStore::open(guard, owner()).unwrap();
    let path = store.path();
    let directory = path.parent().unwrap();
    let moved = directory.with_extension("moved");
    fs::rename(directory, &moved).unwrap();
    fs::create_dir(directory).unwrap();
    assert!(store.append(begin("turn-a", "begin-a", None)).is_err());
    fs::remove_dir(directory).unwrap();
    fs::rename(&moved, directory).unwrap();
    fs::write(root.path().join(".axocoatl-daemon.lock/format.json"), b"{}").unwrap();
    assert!(store.append(begin("turn-a", "begin-a", None)).is_err());
    assert!(!fs::read_to_string(path).unwrap().contains("begin-a"));
}

#[test]
fn existing_recovery_never_creates_a_missing_namespace_or_journal() {
    let root = tempfile::tempdir().unwrap();
    let guard = boundary(&root);
    assert!(SessionExecutionStore::open_existing(guard.clone(), owner()).is_err());
    assert!(!root.path().join("execution-v2").exists());
    let store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    let path = store.path();
    drop(store);
    fs::remove_file(&path).unwrap();
    assert!(SessionExecutionStore::open_existing(guard, owner()).is_err());
    assert!(!path.exists());
}
