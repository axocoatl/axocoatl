use super::*;

fn checkpoint(id: &str, version: u64) -> AgentCheckpoint {
    AgentCheckpoint {
        version,
        agent_id: id.to_owned(),
        checkpoint_time: 1_234,
        session_messages: vec![StoredMessage {
            content_parts: None,
            role: axocoatl_core::MessageRole::User,
            content: format!("private conversation {id}"),
            timestamp: 123,
            token_count: 4,
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        cumulative_token_usage: TokenUsageStats::new(123, 45),
        cumulative_token_usage_known: false,
        behavior_state: Some("{\"private\":\"never infer or silently erase\"}".to_owned()),
    }
}

fn source_file(root: &Path, id: &str, version: u64) -> PathBuf {
    root.join("v1")
        .join(storage_key(id))
        .join(CheckpointStore::checkpoint_name(version))
}

#[tokio::test]
async fn exact_session_capture_preserves_coordinator_workers_private_state_and_unknown_usage() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path(), CheckpointPolicy::Manual);
    for id in [
        "session:coordinator",
        "session:worker:adhoc-1",
        "session-other:worker",
    ] {
        store.save(&checkpoint(id, 1)).await.unwrap();
    }
    let before = std::fs::read(source_file(root.path(), "session:coordinator", 1)).unwrap();
    let snapshot = store.capture_legacy_session_checkpoints("session").unwrap();
    assert_eq!(snapshot.session_id(), "session");
    assert_eq!(snapshot.checkpoints().count(), 2);
    assert!(snapshot.checkpoint("session-other:worker").is_none());
    let coordinator = snapshot.checkpoint("session:coordinator").unwrap();
    assert_eq!(
        coordinator.cumulative_token_usage,
        TokenUsageStats::new(123, 45)
    );
    assert!(!coordinator.cumulative_token_usage_known);
    assert_eq!(
        coordinator.behavior_state,
        checkpoint("session:coordinator", 1).behavior_state
    );
    assert_eq!(
        coordinator.session_messages[0].content,
        "private conversation session:coordinator"
    );
    assert!(snapshot
        .archive_bytes()
        .windows(before.len())
        .any(|part| part == before));
    assert_eq!(
        std::fs::read(source_file(root.path(), "session:coordinator", 1)).unwrap(),
        before
    );
    snapshot.verify_current(&store).unwrap();
}

#[tokio::test]
async fn recapture_detects_new_identity_changed_bytes_and_identical_replaced_inode() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path(), CheckpointPolicy::Manual);
    store.save(&checkpoint("session:agent", 1)).await.unwrap();
    let first = store.capture_legacy_session_checkpoints("session").unwrap();
    store
        .save(&checkpoint("session:new-worker", 1))
        .await
        .unwrap();
    assert!(first.verify_current(&store).is_err());
    let second = store.capture_legacy_session_checkpoints("session").unwrap();
    let path = source_file(root.path(), "session:agent", 1);
    let bytes = std::fs::read(&path).unwrap();
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, &bytes).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    assert!(second.verify_current(&store).is_err());
    let third = store.capture_legacy_session_checkpoints("session").unwrap();
    let mut changed = checkpoint("session:agent", 1);
    changed.cumulative_token_usage.output_tokens += 1;
    std::fs::write(&path, encode_current(&changed).unwrap()).unwrap();
    assert!(third.verify_current(&store).is_err());
}

#[tokio::test]
async fn unresolved_or_scoped_transactions_are_refused_without_repairing_source() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path(), CheckpointPolicy::Manual);
    store.save(&checkpoint("session:agent", 1)).await.unwrap();
    store.begin_session_turn("session", "turn").await.unwrap();
    let path = root
        .path()
        .join(CheckpointStore::transaction_relative("session", "turn"))
        .join(SESSION_TURN_TRANSACTION_MANIFEST);
    let before = std::fs::read(&path).unwrap();
    assert!(store.capture_legacy_session_checkpoints("session").is_err());
    assert!(store
        .scoped_to_session_turn("session", "turn")
        .capture_legacy_session_checkpoints("session")
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    store.commit_session_turn("session", "turn").await.unwrap();
    assert!(store.capture_legacy_session_checkpoints("session").is_ok());
}

#[tokio::test]
async fn corrupt_newest_source_never_falls_back_to_smaller_accounting() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path(), CheckpointPolicy::Manual);
    store.save(&checkpoint("session:agent", 1)).await.unwrap();
    let corrupt = source_file(root.path(), "session:agent", 2);
    std::fs::write(&corrupt, b"unknown newer usage and private state").unwrap();
    assert!(store.capture_legacy_session_checkpoints("session").is_err());
    assert_eq!(
        std::fs::read(&corrupt).unwrap(),
        b"unknown newer usage and private state"
    );
    store.save(&checkpoint("session:agent", 3)).await.unwrap();
    let snapshot = store.capture_legacy_session_checkpoints("session").unwrap();
    assert_eq!(snapshot.checkpoint("session:agent").unwrap().version, 3);
    assert!(snapshot
        .archive_bytes()
        .windows(b"unknown newer usage and private state".len())
        .any(|part| part == b"unknown newer usage and private state"));
}

#[tokio::test]
async fn unknown_hashed_owner_and_unexpected_private_files_are_not_treated_as_absent() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::new(root.path(), CheckpointPolicy::Manual);
    store.save(&checkpoint("session:agent", 1)).await.unwrap();
    let path = source_file(root.path(), "unattributed", 1);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"all records corrupt, owner unknown").unwrap();
    assert!(store.capture_legacy_session_checkpoints("session").is_err());
    std::fs::remove_file(path).unwrap();
    let private =
        source_file(root.path(), "session:agent", 1).with_file_name("unrecognized-state.json");
    std::fs::write(private, b"private state must not disappear").unwrap();
    assert!(store.capture_legacy_session_checkpoints("session").is_err());
}

#[test]
fn capture_does_not_provision_a_missing_source_directory() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-source");
    let store = CheckpointStore::new(&absent, CheckpointPolicy::Manual);
    assert!(store.capture_legacy_session_checkpoints("session").is_err());
    assert!(!absent.exists());
}
