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
    let mut content = store(dir.path(), identity.clone());
    let snapshot = streaming_fixture(&mut canonical, &mut content);
    let event = observation(&snapshot, 0);
    let receipt = content
        .record_activation_stream(&snapshot, event.clone())
        .unwrap();
    let bytes = stored(dir.path());
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
            stored(dir.path()),
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
    let reopened = store(dir.path(), identity);
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
    assert_eq!(stored(dir.path()), bytes);
}

#[test]
fn long_streams_cross_segments_and_terminal_prevents_later_streams() {
    let root = tempfile::tempdir().unwrap();
    let mut canonical = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = store(dir.path(), canonical.identity().unwrap());
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
    // Far more events than one segment holds; each append consults only the
    // event before it, never the whole stream.
    let events = 5 * segments::SPEC.segment_records + 3;
    for sequence in 0..events {
        event.sequence = sequence;
        content
            .record_activation_stream(&snapshot, event.clone())
            .unwrap();
    }
    assert!(content.sealed_segments().0 >= 5);
    // A retained sequence with another body, or a gap, is refused unwritten;
    // an exact repeat is the retained event.
    let mut rewritten = event.clone();
    rewritten.payload = ActivationStreamPayload::Text {
        delta: "rewritten".into(),
    };
    for (sequence, candidate) in [
        (0, &rewritten),
        (1, &rewritten),
        (events / 2, &rewritten),
        (events + 1, &event),
    ] {
        let mut candidate = candidate.clone();
        candidate.sequence = sequence;
        let before = stored(dir.path());
        assert!(matches!(
            content.record_activation_stream(&snapshot, candidate),
            Err(ExecutionContentError::Conflict)
        ));
        assert_eq!(stored(dir.path()), before, "{sequence}");
    }
    event.sequence = 1;
    let before = stored(dir.path());
    content
        .record_activation_stream(&snapshot, event.clone())
        .unwrap();
    assert_eq!(stored(dir.path()), before);
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
    event.sequence = events;
    let before = stored(dir.path());
    assert!(content
        .record_activation_stream(&snapshot, event.clone())
        .is_err());
    assert_eq!(stored(dir.path()), before);
    let identity = canonical.identity().unwrap();
    drop(content);
    let restored = store(dir.path(), identity);
    let retained = restored
        .activation_stream(&snapshot, &event.activation)
        .unwrap();
    assert_eq!(
        retained
            .iter()
            .map(|view| view.content.sequence)
            .collect::<Vec<_>>(),
        (0..events).collect::<Vec<_>>()
    );
    assert_eq!(
        restored.activation_output_settlement(&reservation).unwrap(),
        Some(final_output)
    );
}

#[test]
fn uncertain_stream_acknowledgement_requires_reopen_and_keeps_immutable_event_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut canonical = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = canonical.identity().unwrap();
    let mut content = store(dir.path(), identity.clone());
    let snapshot = streaming_fixture(&mut canonical, &mut content);
    let event = observation(&snapshot, 0);
    assert!(matches!(
        content.append_losing_ack(Body::ActivationStream(event.clone())),
        Err(ExecutionContentError::RecoveryRequired)
    ));
    assert!(content
        .record_activation_stream(&snapshot, event.clone())
        .is_err());
    drop(content);
    let mut restored = store(dir.path(), identity);
    let retained = restored
        .activation_stream(&snapshot, &event.activation)
        .unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(
        restored.record_activation_stream(&snapshot, event).unwrap(),
        retained[0]
    );
}
