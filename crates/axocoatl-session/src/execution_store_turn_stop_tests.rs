use super::*;
use crate::execution_ownership::LegacyFormatOwnership;
use crate::turn_contract::TurnNodeId;
use serde_json::json;

fn fixture() -> (
    tempfile::TempDir,
    SessionExecutionStore,
    Vec<TurnContractEnvelope>,
) {
    let root = tempfile::tempdir().unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let owner = ExecutionStoreOwner {
        workspace_id: "workspace".into(),
        session_id: SessionId::new("session-a").unwrap(),
    };
    let mut store = SessionExecutionStore::open(ownership, owner).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/turn_contract/interrupted_epoch_preserves_acceptance.json"
    ))
    .unwrap();
    let records: Vec<TurnContractEnvelope> = fixture["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
        .collect();
    for record in &records[..2] {
        store.append(record.clone()).unwrap();
    }
    (root, store, records)
}

fn event(revision: u64, body: serde_json::Value) -> TurnContractEnvelope {
    serde_json::from_value(
        json!({"schema_version":2,"command_id":format!("stop-fixture-{revision}"),
        "expected_revision":revision,"session_id":"session-a","turn_id":"turn-a","event":body}),
    )
    .unwrap()
}

#[test]
fn stop_reserves_its_intent_and_late_settlement_at_existing_byte_and_record_boundaries() {
    for scope in 0..2 {
        let (_root, mut store, records) = fixture();
        let TurnContractEvent::StartActivation { input } = &records[1].event else {
            panic!("fixture Start")
        };
        let activation = input.activation.clone();
        store.append(event(2, json!({"kind":"record_intent","invocation_id":"pending-tool","activation":activation}))).unwrap();
        let turn = store.unfinished_turn().unwrap().unwrap().1;
        let (bytes, commands) = settlement_reservation(turn);
        match scope {
            0 => store.limits.turn_bytes = turn.retained_event_bytes() + bytes,
            _ => store.limits.turn_commands = turn.command_count() + commands,
        }
        let unchanged = store.stored_files_for_test();
        assert!(matches!(
            store.append(event(
                3,
                json!({"kind":"record_intent","invocation_id":"too-many","activation":activation})
            )),
            Err(ExecutionStoreError::Capacity)
        ));
        assert_eq!(store.stored_files_for_test(), unchanged);
        let stop = event(
            3,
            json!({"kind":"request_turn_stop","evidence":"retained-human-request"}),
        );
        let request = store.append(stop.clone()).unwrap();
        assert_eq!(
            store.append(stop.clone()).unwrap().sequence(),
            request.sequence()
        );
        for (revision, body) in [
            (
                4,
                json!({"kind":"record_outcome","invocation_id":"pending-tool","outcome":"succeeded","evidence":"actual-result"}),
            ),
            (
                5,
                json!({"kind":"fail_activation","activation":activation,"evidence":"actual-partial-output"}),
            ),
            (6, json!({"kind":"interrupt_epoch","epoch_id":"epoch-1"})),
            (7, json!({"kind":"close","closure":"cancelled"})),
        ] {
            store.append(event(revision, body)).unwrap();
        }
        let snapshot = store
            .snapshot(&LogicalTurnId::new("turn-a").unwrap())
            .unwrap();
        assert_eq!(
            snapshot.contract().state(),
            Some(LogicalTurnState::Cancelled)
        );
        assert_eq!(
            snapshot.contract().invocations()[0].evidence.disposition(),
            EffectDisposition::OutcomeRecorded
        );
        assert_eq!(
            snapshot.contract().stop_requested().unwrap().command_id,
            stop.command_id
        );
    }
}

#[test]
fn stop_fences_future_node_start_continue_and_late_acceptance_without_mutating_initial_graph() {
    let (_root, mut store, records) = fixture();
    let original = store
        .snapshot(&LogicalTurnId::new("turn-a").unwrap())
        .unwrap();
    let stop = event(
        2,
        json!({"kind":"request_turn_stop","evidence":"retained-human-request"}),
    );
    let bytes = serde_json::to_vec(&stop).unwrap();
    assert_eq!(TurnContractEnvelope::decode(&bytes).unwrap(), stop);
    store.append(stop).unwrap();
    let stopped = store.snapshot(original.turn_id()).unwrap();
    assert_eq!(stopped.contract().graph(), original.contract().graph());
    assert!(stopped
        .contract()
        .stop_requested()
        .unwrap()
        .unrun_nodes
        .contains(&TurnNodeId::new("node-b").unwrap()));
    let before = store.stored_files_for_test();
    // Every later Start/Accept from the fixture would be forbidden even when
    // its exact original body is rebound to the currently expected revision.
    for record in &records[2..] {
        if matches!(
            record.event,
            TurnContractEvent::StartActivation { .. }
                | TurnContractEvent::AcceptActivation { .. }
                | TurnContractEvent::Continue { .. }
        ) {
            let mut rejected = record.clone();
            rejected.expected_revision = stopped.contract().revision();
            assert!(store.append(rejected).is_err());
            assert_eq!(store.stored_files_for_test(), before);
        }
    }
    assert!(store
        .append(event(3, json!({"kind":"close","closure":"completed"})))
        .is_err());
    assert!(store
        .append(event(3, json!({"kind":"close","closure":"finished"})))
        .is_err());
    assert_eq!(store.stored_files_for_test(), before);
    // The old serialization remains unchanged until a Stop has actually been recorded.
    assert!(serde_json::to_value(original.contract())
        .unwrap()
        .get("stop_requested")
        .is_none());
}
