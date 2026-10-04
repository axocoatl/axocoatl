//! The canonical journal across sealed segments: lookups and folds of old
//! turns, migration from the single-file layout, and recovery.
use super::*;
use crate::execution_ownership::LegacyFormatOwnership;
use serde_json::json;

fn owner() -> ExecutionStoreOwner {
    ExecutionStoreOwner {
        workspace_id: "workspace".into(),
        session_id: SessionId::new("session-a").unwrap(),
    }
}

fn ownership(root: &tempfile::TempDir) -> Arc<UpgradedFormatOwnership> {
    Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    )
}

fn envelope(turn: &str, revision: u64, id: &str, event: serde_json::Value) -> TurnContractEnvelope {
    serde_json::from_value(json!({
        "schema_version": 2, "command_id": id, "expected_revision": revision,
        "session_id": "session-a", "turn_id": turn, "event": event,
    }))
    .unwrap()
}

fn begin(turn: &str, predecessor: Option<&ClosedTurnRef>) -> TurnContractEnvelope {
    envelope(
        turn,
        0,
        &format!("begin-{turn}"),
        json!({"kind": "begin", "epoch_id": "epoch-1", "predecessor": predecessor,
            "graph": {"snapshot_id": "graph-v1", "revision": 1, "dependencies": [], "conditions": [],
                "nodes": [{"node_id": "tester", "slot_id": "qa",
                    "definition": {"definition_id": "tester", "snapshot": "definition-v1"},
                    "conversation_id": "qa-conversation", "starting_savepoint": {"kind": "empty"},
                    "required": true}]}}),
    )
}

fn close(turn: &str) -> TurnContractEnvelope {
    envelope(
        turn,
        1,
        &format!("close-{turn}"),
        json!({"kind": "close", "closure": "finished"}),
    )
}

/// `count` closed turns, each naming the one before.
fn turns(store: &mut SessionExecutionStore, count: usize) {
    let mut predecessor: Option<ClosedTurnRef> = None;
    for n in 0..count {
        let turn = format!("turn-{n}");
        store.append(begin(&turn, predecessor.as_ref())).unwrap();
        store.append(close(&turn)).unwrap();
        predecessor = Some(
            store
                .turn(&LogicalTurnId::new(&turn).unwrap())
                .unwrap()
                .unwrap()
                .closed_reference()
                .unwrap(),
        );
    }
}

#[test]
fn old_turns_are_folded_and_found_again_from_sealed_segments() {
    let root = tempfile::tempdir().unwrap();
    let guard = ownership(&root);
    let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    turns(&mut store, 30);
    // Eight records per sealed segment in unit tests.
    assert!(
        store.log.sealed().len() >= 7,
        "{}",
        store.log.sealed().len()
    );
    assert!(store.active.len() < 8);
    assert!(store.live.is_none());
    let check = |store: &SessionExecutionStore| {
        assert_eq!(store.turn_ids().unwrap().len(), 30);
        for n in [0, 7, 29] {
            let id = LogicalTurnId::new(format!("turn-{n}")).unwrap();
            let snapshot = store.snapshot(&id).unwrap();
            assert_eq!(
                snapshot.contract().state(),
                Some(LogicalTurnState::Finished)
            );
            let records = store.turn_records(&id).unwrap();
            assert_eq!(records.len(), 2);
            assert_eq!(
                records[0],
                begin(id.as_str(), snapshot.contract().predecessor())
            );
            let (sequence, record) = store
                .command_record(&CommandId::new(format!("close-turn-{n}")).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(sequence, n as u64 * 2 + 2);
            assert_eq!(record, close(id.as_str()));
        }
        assert!(store
            .command_record(&CommandId::new("absent").unwrap())
            .unwrap()
            .is_none());
        assert_eq!(store.records().unwrap().len(), 60);
        assert_eq!(store.records_in(5, 6).unwrap().len(), 2);
    };
    check(&store);
    drop(store);
    let mut store = SessionExecutionStore::open(guard, owner()).unwrap();
    check(&store);
    // A replayed old command keeps its receipt; a changed one is refused,
    // and so is any change to a turn that already ended.
    let receipt = store.append(close("turn-3")).unwrap();
    assert_eq!(receipt.sequence(), 8);
    let mut changed = close("turn-3");
    changed.expected_revision = 2;
    assert!(matches!(
        store.append(changed),
        Err(ExecutionStoreError::Contract(
            TurnContractError::CommandConflict
        ))
    ));
    let late = envelope(
        "turn-3",
        2,
        "late",
        json!({"kind": "close", "closure": "finished"}),
    );
    assert!(store.append(late).is_err());
    assert_eq!(store.record_count(), 60);
}

/// A journal written in the single-file layout, records and request
/// bindings in one file, opens with its history intact and is converted;
/// a conversion cut short is redone.
#[test]
fn a_single_file_journal_is_migrated_into_segments() {
    let root = tempfile::tempdir().unwrap();
    let guard = ownership(&root);
    let store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    let dir = store.dir.clone();
    let head = store.head.clone();
    drop(store);
    let mut records = vec![];
    let mut predecessor: Option<ClosedTurnRef> = None;
    for n in 0..12 {
        let turn = format!("turn-{n}");
        let mut fold = TurnContract::default();
        let first = begin(&turn, predecessor.as_ref());
        let second = close(&turn);
        fold.apply(&first).unwrap();
        fold.apply(&second).unwrap();
        predecessor = Some(fold.closed_reference().unwrap());
        records.push(first);
        records.push(second);
    }
    let requests = vec![RequestBinding {
        turn_id: LogicalTurnId::new("turn-4").unwrap(),
        reference: EvidenceRef::new("request-4").unwrap(),
    }];
    let legacy = Journal {
        records: records.clone(),
        requests,
        segments: None,
        ..head
    };
    let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
    SegmentLog::remove(&dir, &SPEC).unwrap();
    dir.atomic_write(FILE, &legacy_bytes).unwrap();
    // A partial conversion left behind is discarded.
    let partial = SegmentLog::open(
        dir.clone(),
        SPEC,
        json!({"partial": true}),
        true,
        |_, _: CanonicalRecord| Ok::<(), SegmentError>(()),
    )
    .unwrap();
    drop(partial);
    // The directory handle holds the Session lock; release it first.
    drop(dir);
    let store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    assert_eq!(store.records().unwrap(), records);
    let fourth = store
        .snapshot(&LogicalTurnId::new("turn-4").unwrap())
        .unwrap();
    assert_eq!(fourth.request_ref().unwrap().as_str(), "request-4");
    assert!(store.log.sealed().len() >= 2);
    drop(store);
    let reopened = SessionExecutionStore::open(guard, owner()).unwrap();
    let head: serde_json::Value =
        serde_json::from_slice(&std::fs::read(reopened.path()).unwrap()).unwrap();
    assert!(head.get("records").unwrap().as_array().unwrap().is_empty());
    assert_eq!(head["segments"]["kind"], "execution.v2");
    assert_eq!(reopened.records().unwrap(), records);
    assert_eq!(reopened.turn_ids().unwrap().len(), 12);
}

/// A record cut short by a crash was never acknowledged: reopening drops
/// it, and the journal continues from the last whole record.
#[test]
fn a_torn_record_is_dropped_on_reopen() {
    let root = tempfile::tempdir().unwrap();
    let guard = ownership(&root);
    let mut store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    turns(&mut store, 3);
    let active = store.dir.path().join(SPEC.active_name());
    let count = store.record_count();
    drop(store);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        br#"{"record":{"kind":"event","envelope":{"schema"#,
    )
    .unwrap();
    drop(file);
    let mut store = SessionExecutionStore::open(guard, owner()).unwrap();
    assert_eq!(store.record_count(), count);
    let predecessor = store
        .turn(&LogicalTurnId::new("turn-2").unwrap())
        .unwrap()
        .unwrap()
        .closed_reference()
        .unwrap();
    store.append(begin("turn-3", Some(&predecessor))).unwrap();
    assert_eq!(store.record_count(), count + 1);
}
