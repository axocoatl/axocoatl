use super::*;

fn streaming_fixture(
    canonical: &mut SessionExecutionStore,
    content: &mut ExecutionContentStore,
) -> DurableTurnSnapshot {
    let (_, start) = proposed_input_fixture(canonical, content);
    let turn_id = start.turn_id.clone();
    canonical.append(start).unwrap();
    canonical.snapshot(&turn_id).unwrap()
}
fn observation(snapshot: &DurableTurnSnapshot, sequence: u64) -> ActivationStreamContent {
    ActivationStreamContent {
        schema_version: 1,
        activation: snapshot.contract().activations()[0].activation.clone(),
        sequence,
        recorded_at_unix_ms: 100,
        payload: ActivationStreamPayload::Text {
            delta: "observed 🐍".into(),
        },
    }
}

#[test]
fn stream_identity_sequence_and_reopen_preserve_exact_observations_without_acceptance() {
    let root = tempfile::tempdir().unwrap();
    let mut canonical = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = canonical.identity().unwrap();
    let mut content = store(dir.path(), identity.clone(), Limits::default());
    let snapshot = streaming_fixture(&mut canonical, &mut content);
    let event = observation(&snapshot, 0);
    let receipt = content
        .record_activation_stream(&snapshot, event.clone())
        .unwrap();
    let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
    assert_eq!(
        content
            .record_activation_stream(&snapshot, event.clone())
            .unwrap(),
        receipt
    );
    for case in ["sequence", "body", "activation", "epoch", "version"] {
        let mut invalid = event.clone();
        match case {
            "sequence" => invalid.sequence = 2,
            "body" => {
                invalid.payload = ActivationStreamPayload::Text {
                    delta: "rewritten".into(),
                }
            }
            "activation" => {
                invalid.activation.activation_id = ActivationId::new("foreign").unwrap()
            }
            "epoch" => {
                invalid.activation.execution_epoch_id = ExecutionEpochId::new("foreign").unwrap()
            }
            "version" => invalid.schema_version = 2,
            _ => unreachable!(),
        }
        assert!(
            content
                .record_activation_stream(&snapshot, invalid)
                .is_err(),
            "{case}"
        );
        assert_eq!(
            std::fs::read(dir.path().join(FILE)).unwrap(),
            bytes,
            "{case}"
        );
    }
    assert_eq!(
        canonical
            .snapshot(snapshot.turn_id())
            .unwrap()
            .contract()
            .activations()[0]
            .state,
        ActivationState::Running
    );
    drop(content);
    let reopened = store(dir.path(), identity, Limits::default());
    assert_eq!(
        reopened
            .activation_stream(&snapshot, &event.activation)
            .unwrap(),
        vec![receipt]
    );
    assert!(
        reopened.project(&snapshot).unwrap().activations[0].output
            == ContentResolution::NotRecorded
    );
    assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), bytes);
}

#[test]
fn stream_capacity_cannot_spend_terminal_reservation_and_terminal_prevents_later_streams() {
    for count_limit in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let mut canonical = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let mut content = store(dir.path(), canonical.identity().unwrap(), Limits::default());
        let snapshot = streaming_fixture(&mut canonical, &mut content);
        let mut event = observation(&snapshot, 0);
        let reservation = content
            .reserve_activation_output(
                &snapshot,
                &event.activation,
                ActivationOutputLimits {
                    partial_records: 0,
                    partial_bytes: 0,
                    settlement_bytes: 128,
                },
            )
            .unwrap();
        if count_limit {
            content.limits.records = content.data.records.len() + 2;
        } else {
            // A known finite byte allowance, with the actual same terminal
            // reservation charged by validate_capacity, rather than a mock gate.
            content.limits.bytes = encode_bounded(&content.data, MAX_BYTES).unwrap().len()
                + 128 * 6
                + OUTPUT_OVERHEAD
                + 2048;
        }
        let mut admitted = 0;
        loop {
            event.sequence = admitted;
            let before = std::fs::read(dir.path().join(FILE)).unwrap();
            match content.record_activation_stream(&snapshot, event.clone()) {
                Ok(_) => {
                    admitted += 1;
                    assert!(admitted < 20);
                }
                Err(ExecutionContentError::Capacity) => {
                    assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
                    break;
                }
                other => panic!("unexpected stream admission: {other:?}"),
            }
        }
        assert!(admitted > 0);
        let final_output = content
            .settle_activation_output(
                &reservation,
                ActivationOutputContent {
                    activation: event.activation.clone(),
                    recorded_at_unix_ms: 101,
                    text: "terminal partial".into(),
                    kind: OutputKind::Partial,
                    usage: ExecutionUsage::Unknown {
                        known_subtotal: TokenUsageStats::new(2, 0),
                    },
                },
            )
            .unwrap();
        assert!(final_output.complete_output().is_none());
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(content
            .record_activation_stream(&snapshot, event.clone())
            .is_err());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        let identity = canonical.identity().unwrap();
        drop(content);
        let restored = store(dir.path(), identity, Limits::default());
        assert_eq!(
            restored
                .activation_stream(&snapshot, &event.activation)
                .unwrap()
                .len(),
            admitted as usize
        );
        assert_eq!(
            restored.activation_output_settlement(&reservation).unwrap(),
            Some(final_output)
        );
    }
}

#[test]
fn uncertain_stream_acknowledgement_requires_reopen_and_keeps_immutable_event_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut canonical = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = canonical.identity().unwrap();
    let mut content = store(dir.path(), identity.clone(), Limits::default());
    let snapshot = streaming_fixture(&mut canonical, &mut content);
    let event = observation(&snapshot, 0);
    assert!(matches!(
        content.append_with(Body::ActivationStream(event.clone()), |storage, bytes| {
            storage.write(bytes)?;
            Err(io::Error::other("lost stream acknowledgement"))
        }),
        Err(ExecutionContentError::RecoveryRequired)
    ));
    assert!(content
        .record_activation_stream(&snapshot, event.clone())
        .is_err());
    drop(content);
    let mut restored = store(dir.path(), identity, Limits::default());
    let retained = restored
        .activation_stream(&snapshot, &event.activation)
        .unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(
        restored.record_activation_stream(&snapshot, event).unwrap(),
        retained[0]
    );
}
