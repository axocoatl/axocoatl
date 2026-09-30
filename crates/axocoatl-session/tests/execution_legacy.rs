use std::fs;
use std::sync::Arc;

use axocoatl_session::execution_content::{ExecutionContentError, ExecutionContentStore};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{
    ExecutionStoreError, ExecutionStoreOwner, SessionExecutionStore,
};
use axocoatl_session::turn_contract::SessionId;
use axocoatl_session::{
    BeginSessionTurn, SessionTurn, SessionTurnLifecycle, SessionTurnStore, TransitionSessionTurn,
};

fn row(session: &str, id: &str) -> SessionTurn {
    SessionTurn {
        id: id.into(),
        session_id: session.into(),
        user_input: format!("request {id}"),
        agent_id: Some("coder".into()),
        model: None,
        context: Vec::new(),
        status: SessionTurnLifecycle::Completed,
        partial_output: String::new(),
        final_output: Some(format!("answer {id}")),
        error: None,
        created_at: 1,
        updated_at: 2,
        completed_at: Some(2),
        idempotency_key: None,
        metadata: Default::default(),
        execution_events: Vec::new(),
        agent_outputs: Vec::new(),
        superseded: false,
    }
}

fn canonical(root: &tempfile::TempDir) -> SessionExecutionStore {
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    SessionExecutionStore::open(
        ownership,
        ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session").unwrap(),
        },
    )
    .unwrap()
}

fn content(store: &SessionExecutionStore) -> ExecutionContentStore {
    ExecutionContentStore::open_owned(
        store
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn missing_source_is_distinct_from_an_observed_empty_ledger() {
    let root = tempfile::tempdir().unwrap();
    let mut store = canonical(&root);
    assert!(store.legacy_history_snapshot().is_err());
    assert!(!root.path().join("session-history").exists());
    SessionTurnStore::open(root.path().join("session-history")).unwrap();
    let snapshot = store.legacy_history_snapshot().unwrap();
    assert_eq!(snapshot.source().byte_len, 0);
    assert!(snapshot.turns().is_empty());
    let mut content = content(&store);
    let retained = content.retain_legacy_history(&snapshot).unwrap();
    let seal = store.seal_legacy_history(&retained).unwrap();
    assert!(content.read_legacy_history(&seal).unwrap().turns.is_empty());
}

#[test]
fn snapshot_reloads_the_owned_file_instead_of_a_stale_or_foreign_fold() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session-history");
    let mut stale = SessionTurnStore::open(&path).unwrap();
    stale
        .import_terminal_turns("session", "first", vec![row("session", "first")])
        .unwrap();
    let store = canonical(&root);
    let mut actual = SessionTurnStore::open(&path).unwrap();
    actual
        .begin(BeginSessionTurn {
            turn_id: Some("second".into()),
            session_id: "session".into(),
            user_input: "request second".into(),
            agent_id: Some("coder".into()),
            model: None,
            context: Vec::new(),
            idempotency_key: None,
            metadata: Default::default(),
        })
        .unwrap();
    actual
        .transition(
            "second",
            "second-completed",
            TransitionSessionTurn {
                status: SessionTurnLifecycle::Completed,
                final_output: Some("answer second".into()),
                error: None,
                metadata: Default::default(),
            },
        )
        .unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut foreign = SessionTurnStore::open(other.path()).unwrap();
    foreign
        .import_terminal_turns("session", "foreign", vec![row("session", "forged")])
        .unwrap();
    assert_eq!(stale.list("session").len(), 1);
    let snapshot = store.legacy_history_snapshot().unwrap();
    assert_eq!(
        snapshot
            .turns()
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_ne!(snapshot.turns(), foreign.list("session"));
}

#[test]
fn incomplete_and_unsupported_sources_are_refused_without_repair() {
    for fault in ["partial", "zero-schema", "future-schema", "malformed"] {
        let root = tempfile::tempdir().unwrap();
        let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
        legacy
            .import_terminal_turns("session", "first", vec![row("session", "first")])
            .unwrap();
        let mut bytes = fs::read(legacy.path()).unwrap();
        match fault {
            "partial" => bytes.extend_from_slice(b"{\"schema_version\":"),
            "malformed" => bytes.extend_from_slice(b"not a valid event\n"),
            _ => {
                let mut event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                event["schema_version"] = if fault == "zero-schema" {
                    0.into()
                } else {
                    2.into()
                };
                bytes = serde_json::to_vec(&event).unwrap();
                bytes.push(b'\n');
            }
        }
        fs::write(legacy.path(), &bytes).unwrap();
        let store = canonical(&root);
        assert!(store.legacy_history_snapshot().is_err(), "{fault}");
        assert_eq!(fs::read(legacy.path()).unwrap(), bytes, "{fault}");
    }
}

#[test]
fn stale_snapshot_cannot_be_retained() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    let store = canonical(&root);
    let snapshot = store.legacy_history_snapshot().unwrap();
    legacy
        .import_terminal_turns("session", "first", vec![row("session", "first")])
        .unwrap();
    assert!(content(&store).retain_legacy_history(&snapshot).is_err());
}

#[test]
fn source_changed_after_retention_cannot_be_sealed() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    let mut store = canonical(&root);
    let mut content = content(&store);
    let retained = content
        .retain_legacy_history(&store.legacy_history_snapshot().unwrap())
        .unwrap();
    legacy
        .import_terminal_turns("session", "first", vec![row("session", "first")])
        .unwrap();
    assert!(store.seal_legacy_history(&retained).is_err());
    assert!(store.legacy_seal().unwrap().is_none());
    assert!(store.records().unwrap().is_empty());
    // A failed seal must not strand migration on an obsolete retained candidate.
    let fresh = content
        .retain_legacy_history(&store.legacy_history_snapshot().unwrap())
        .unwrap();
    let seal = store.seal_legacy_history(&fresh).unwrap();
    assert_eq!(content.read_legacy_history(&seal).unwrap().turns.len(), 1);
}

#[test]
fn exact_seal_retry_keeps_immutable_history_after_later_legacy_mutation() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    let mut store = canonical(&root);
    let mut content = content(&store);
    let retained = content
        .retain_legacy_history(&store.legacy_history_snapshot().unwrap())
        .unwrap();
    let seal = store.seal_legacy_history(&retained).unwrap();
    legacy
        .import_terminal_turns("session", "later", vec![row("session", "later")])
        .unwrap();
    assert_eq!(store.seal_legacy_history(&retained).unwrap(), seal);
    assert!(content.read_legacy_history(&seal).unwrap().turns.is_empty());
}

#[test]
fn foreign_snapshot_and_same_bytes_replacement_do_not_prove_ownership() {
    let root = tempfile::tempdir().unwrap();
    let legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    let store = canonical(&root);
    let mut content = content(&store);
    let foreign_root = tempfile::tempdir().unwrap();
    SessionTurnStore::open(foreign_root.path().join("session-history")).unwrap();
    let foreign = canonical(&foreign_root);
    assert!(matches!(
        content.retain_legacy_history(&foreign.legacy_history_snapshot().unwrap()),
        Err(ExecutionContentError::OwnerMismatch)
    ));
    let snapshot = store.legacy_history_snapshot().unwrap();
    let replacement = legacy.path().with_extension("replacement");
    fs::write(&replacement, fs::read(legacy.path()).unwrap()).unwrap();
    fs::rename(&replacement, legacy.path()).unwrap();
    assert!(content.retain_legacy_history(&snapshot).is_err());
}

#[test]
fn history_limit_applies_to_the_selected_session() {
    let root = tempfile::tempdir().unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    legacy
        .import_terminal_turns(
            "other",
            "other-history",
            (0..2049)
                .map(|i| row("other", &format!("other-{i}")))
                .collect(),
        )
        .unwrap();
    legacy
        .import_terminal_turns("session", "selected", vec![row("session", "selected")])
        .unwrap();
    let store = canonical(&root);
    let snapshot = store.legacy_history_snapshot().unwrap();
    assert_eq!(snapshot.turns().len(), 1);
    assert_eq!(snapshot.turns()[0].id, "selected");
    content(&store).retain_legacy_history(&snapshot).unwrap();
    let oversized_root = tempfile::tempdir().unwrap();
    let mut oversized =
        SessionTurnStore::open(oversized_root.path().join("session-history")).unwrap();
    oversized
        .import_terminal_turns(
            "session",
            "too-many",
            (0..2049)
                .map(|i| row("session", &format!("selected-{i}")))
                .collect(),
        )
        .unwrap();
    assert!(matches!(
        canonical(&oversized_root).legacy_history_snapshot(),
        Err(ExecutionStoreError::Capacity)
    ));
}
