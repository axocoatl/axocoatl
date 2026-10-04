#![cfg(unix)]

use std::fs;
use std::sync::Arc;

use axocoatl_core::TokenUsageStats;
use axocoatl_session::control_authority::{AuthorityGrant, ExecutionProfile, GrantLimits};
use axocoatl_session::execution_content::*;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::*;
use axocoatl_session::turn_contract::*;
use axocoatl_session::{
    BeginSessionTurn, SessionTurnLifecycle, SessionTurnStore, TransitionSessionTurn,
};

fn owner() -> ExecutionStoreOwner {
    ExecutionStoreOwner {
        workspace_id: "workspace-a".into(),
        session_id: SessionId::new("session-a").unwrap(),
    }
}

fn canonical(root: &tempfile::TempDir) -> (Arc<UpgradedFormatOwnership>, SessionExecutionStore) {
    let guard = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let store = SessionExecutionStore::open(guard.clone(), owner()).unwrap();
    (guard, store)
}

fn initial_events() -> Vec<TurnContractEnvelope> {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/turn_contract/partial_finish_is_not_success.json"
    ))
    .unwrap();
    fixture["steps"].as_array().unwrap()[..2]
        .iter()
        .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
        .collect()
}

fn request() -> ExecutionRequestContent {
    ExecutionRequestContent {
        turn_id: LogicalTurnId::new("turn-a").unwrap(),
        recorded_at_unix_ms: 1789192800000,
        display_input: "Check this repository".into(),
        effective_input: "Check this repository\nExact retained context".into(),
        context: vec![],
        target_definition: Some(AgentDefinitionId::new("shared-coder").unwrap()),
        model: Some(ExecutionModelRef {
            provider_id: "provider-a".into(),
            model_id: "model-a".into(),
            configuration_ref: EvidenceRef::new("model-config-a").unwrap(),
        }),
    }
}

fn begin_running(
    store: &mut SessionExecutionStore,
    content: &mut ExecutionContentStore,
) -> DurableTurnSnapshot {
    let events = initial_events();
    let receipt = content.retain_request(request()).unwrap();
    store
        .begin_with_request(events[0].clone(), &receipt)
        .unwrap();
    store.append(events[1].clone()).unwrap();
    store.snapshot(&request().turn_id).unwrap()
}

fn event(
    snapshot: &DurableTurnSnapshot,
    name: &str,
    event: TurnContractEvent,
) -> TurnContractEnvelope {
    TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new(name).unwrap(),
        expected_revision: snapshot.contract().revision(),
        session_id: owner().session_id,
        turn_id: snapshot.turn_id().clone(),
        event,
    }
}

fn output(snapshot: &DurableTurnSnapshot, kind: OutputKind) -> ActivationOutputContent {
    ActivationOutputContent {
        activation: snapshot.contract().activations()[0].activation.clone(),
        recorded_at_unix_ms: 1789192800100,
        text: "Observed exact output 🦎".into(),
        usage: ExecutionUsage::Unknown {
            known_subtotal: TokenUsageStats {
                input_tokens: 23,
                output_tokens: 7,
                reasoning_tokens: None,
            },
        },
        kind,
    }
}

#[test]
fn reserved_output_retains_utf8_truncation_usage_and_exact_slot_idempotency() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let activation = snapshot.contract().activations()[0].activation.clone();
    let limits = ActivationOutputLimits {
        partial_records: 1,
        partial_bytes: 5,
        settlement_bytes: 5,
    };
    let reservation = content
        .reserve_activation_output(&snapshot, &activation, limits)
        .unwrap();
    assert_eq!(
        content
            .reserve_activation_output(&snapshot, &activation, limits)
            .unwrap(),
        reservation
    );
    assert!(matches!(
        content.reserve_activation_output(
            &snapshot,
            &activation,
            ActivationOutputLimits {
                settlement_bytes: 6,
                ..limits
            }
        ),
        Err(ExecutionContentError::Conflict)
    ));
    let mut partial = output(&snapshot, OutputKind::Partial);
    partial.text = "é🦎 evidence".into();
    let recorded = content
        .record_activation_partial(&reservation, 0, partial.clone())
        .unwrap();
    assert_eq!(recorded.content().output.text, "é");
    assert!(recorded.content().is_truncated());
    assert_eq!(
        recorded.content().original_byte_len,
        partial.text.len() as u64
    );
    assert_eq!(recorded.content().output.usage, partial.usage);
    assert_eq!(
        content
            .record_activation_partial(&reservation, 0, partial.clone())
            .unwrap(),
        recorded
    );
    assert!(matches!(
        content.record_activation_partial(&reservation, 1, partial.clone()),
        Err(ExecutionContentError::Capacity)
    ));
    assert!(matches!(
        content.retain_output(&snapshot, partial.clone()),
        Err(ExecutionContentError::Conflict)
    ));
    let mut final_output = partial;
    final_output.kind = OutputKind::Final;
    let final_receipt = content
        .settle_activation_output(&reservation, final_output.clone())
        .unwrap();
    assert!(final_receipt.complete_output().is_none());
    assert_eq!(
        content
            .settle_activation_output(&reservation, final_output)
            .unwrap(),
        final_receipt
    );
    let view = content.project(&snapshot).unwrap();
    assert_eq!(view.activations[0].reserved_outputs.len(), 2);
    assert!(matches!(
        view.activations[0].output,
        ContentResolution::NotRecorded
    ));
    assert!(view.activations[0].partial_outputs.is_empty());
    assert!(!view.activations[0].currently_accepted);
}

#[test]
fn reserved_complete_final_resolves_only_after_canonical_acceptance() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let activation = snapshot.contract().activations()[0].activation.clone();
    let reservation = content
        .reserve_activation_output(
            &snapshot,
            &activation,
            ActivationOutputLimits {
                partial_records: 0,
                partial_bytes: 0,
                settlement_bytes: 1024,
            },
        )
        .unwrap();
    let final_output = output(&snapshot, OutputKind::Final);
    let settled = content
        .settle_activation_output(&reservation, final_output.clone())
        .unwrap();
    let accepted = settled.complete_output().unwrap();
    assert!(matches!(
        content.project(&snapshot).unwrap().activations[0].output,
        ContentResolution::NotRecorded
    ));
    store
        .append(event(
            &snapshot,
            "accept-reserved-final",
            TurnContractEvent::AcceptActivation {
                activation: activation.clone(),
                checkpoint: Box::new(CheckpointRef {
                    checkpoint_id: CheckpointId::new("reserved-checkpoint").unwrap(),
                    session_id: owner().session_id,
                    conversation_id: snapshot.contract().activations()[0].conversation_id.clone(),
                    source: CheckpointSource::Accepted { activation },
                }),
                output: accepted.reference().clone(),
            },
        ))
        .unwrap();
    let view = content
        .project(&store.snapshot(&request().turn_id).unwrap())
        .unwrap();
    assert!(view.activations[0].currently_accepted);
    assert!(
        matches!(&view.activations[0].output, ContentResolution::Available { content, .. } if content == &final_output)
    );
    assert_eq!(
        view.activations[0].reserved_outputs[0].content.output.usage,
        final_output.usage
    );
}

#[test]
fn reservation_reopens_for_late_terminal_partial_without_new_admission_or_closure_change() {
    let root = tempfile::tempdir().unwrap();
    let (guard, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = store.identity().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let activation = snapshot.contract().activations()[0].activation.clone();
    let limits = ActivationOutputLimits {
        partial_records: 0,
        partial_bytes: 0,
        settlement_bytes: 128,
    };
    let reservation = content
        .reserve_activation_output(&snapshot, &activation, limits)
        .unwrap();
    drop(content);
    drop(store);
    let mut store = SessionExecutionStore::open(guard, owner()).unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), identity).unwrap();
    let interrupted = store.snapshot(&request().turn_id).unwrap();
    assert_eq!(
        interrupted.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert_eq!(
        content
            .activation_output_reservation(&interrupted, &activation)
            .unwrap(),
        Some(reservation.clone())
    );
    assert_eq!(
        content
            .reserve_activation_output(&interrupted, &activation, limits)
            .unwrap(),
        reservation
    );
    store
        .append(event(
            &interrupted,
            "cancel-with-reservation",
            TurnContractEvent::Close {
                closure: TurnClosure::Cancelled,
            },
        ))
        .unwrap();
    let closed = store.snapshot(&request().turn_id).unwrap();
    let settled = content
        .settle_activation_output(&reservation, output(&closed, OutputKind::Partial))
        .unwrap();
    assert!(settled.complete_output().is_none());
    assert_eq!(
        closed.contract(),
        store.snapshot(&request().turn_id).unwrap().contract()
    );
    assert_eq!(
        content.activation_output_settlement(&reservation).unwrap(),
        Some(settled)
    );

    let other_dir = tempfile::tempdir().unwrap();
    let mut no_reservation =
        ExecutionContentStore::open(other_dir.path(), store.identity().unwrap()).unwrap();
    assert!(matches!(
        no_reservation.reserve_activation_output(&closed, &activation, limits),
        Err(ExecutionContentError::Invalid(_))
    ));
    assert!(matches!(
        no_reservation.settle_activation_output(&reservation, output(&closed, OutputKind::Partial)),
        Err(ExecutionContentError::Conflict)
    ));
}

#[test]
fn foreign_output_reservation_and_truncated_acceptance_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let activation = snapshot.contract().activations()[0].activation.clone();
    let reservation = content
        .reserve_activation_output(
            &snapshot,
            &activation,
            ActivationOutputLimits {
                partial_records: 0,
                partial_bytes: 0,
                settlement_bytes: 1,
            },
        )
        .unwrap();
    let foreign_root = tempfile::tempdir().unwrap();
    let (_, foreign_store) = canonical(&foreign_root);
    let foreign_dir = tempfile::tempdir().unwrap();
    let mut foreign =
        ExecutionContentStore::open(foreign_dir.path(), foreign_store.identity().unwrap()).unwrap();
    assert!(matches!(
        foreign.settle_activation_output(&reservation, output(&snapshot, OutputKind::Final)),
        Err(ExecutionContentError::OwnerMismatch)
    ));
    let final_output = content
        .settle_activation_output(&reservation, output(&snapshot, OutputKind::Final))
        .unwrap();
    assert!(final_output.complete_output().is_none());
    // A caller can submit an ordinary EvidenceRef to the fold, but physical
    // resolution must reject a truncated prefix as complete accepted output.
    store
        .append(event(
            &snapshot,
            "accept-truncated",
            TurnContractEvent::AcceptActivation {
                activation: activation.clone(),
                checkpoint: Box::new(CheckpointRef {
                    checkpoint_id: CheckpointId::new("truncated-checkpoint").unwrap(),
                    session_id: owner().session_id,
                    conversation_id: snapshot.contract().activations()[0].conversation_id.clone(),
                    source: CheckpointSource::Accepted { activation },
                }),
                output: final_output.reference().clone(),
            },
        ))
        .unwrap();
    assert!(matches!(
        content.project(&store.snapshot(&request().turn_id).unwrap()),
        Err(ExecutionContentError::Conflict)
    ));
}

#[test]
fn retained_request_is_immutable_and_canonical_projection_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let (guard, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = store.identity().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
    let receipt = content.retain_request(request()).unwrap();
    assert_eq!(receipt, content.retain_request(request()).unwrap());
    let mut changed = request();
    changed.effective_input.push_str(" changed");
    assert!(matches!(
        content.retain_request(changed),
        Err(ExecutionContentError::Conflict)
    ));
    let snapshot = begin_running(&mut store, &mut content);
    let view = content.project(&snapshot).unwrap();
    assert_eq!(view.state, LogicalTurnState::Running);
    assert!(
        matches!(view.request, ContentResolution::Available { content, .. } if content == request())
    );
    drop(content);
    drop(store);
    let recovered = SessionExecutionStore::open(guard, owner()).unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), identity).unwrap();
    assert_eq!(content.retain_request(request()).unwrap(), receipt);
    let view = content
        .project(&recovered.snapshot(&request().turn_id).unwrap())
        .unwrap();
    assert_eq!(view.state, LogicalTurnState::NeedsAttention);
    assert_eq!(view.epochs[0].state, EpochState::Interrupted);
    assert!(matches!(view.request, ContentResolution::Available { .. }));
}

#[test]
fn foreign_journal_and_replacement_content_directory_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    assert!(ExecutionContentStore::open(dir.path(), store.identity().unwrap()).is_err());
    let other_root = tempfile::tempdir().unwrap();
    let (_, other) = canonical(&other_root);
    let other_dir = tempfile::tempdir().unwrap();
    let foreign = ExecutionContentStore::open(other_dir.path(), other.identity().unwrap()).unwrap();
    assert!(matches!(
        foreign.project(&snapshot),
        Err(ExecutionContentError::OwnerMismatch)
    ));
    drop(content);
    assert!(matches!(
        ExecutionContentStore::open(dir.path(), other.identity().unwrap()),
        Err(ExecutionContentError::OwnerMismatch)
    ));
}

#[test]
fn protected_content_refuses_preexisting_shared_writable_directory() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (_, store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ExecutionContentStore::open(dir.path(), store.identity().unwrap()).is_err());
    assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn request_retained_before_begin_is_recoverable_without_fabricating_new_timestamp() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = store.identity().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
    let receipt = content.retain_request(request()).unwrap();
    drop(content);
    let content = ExecutionContentStore::open(dir.path(), identity).unwrap();
    let (restored, body) = content
        .retained_request(&request().turn_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored, receipt);
    assert_eq!(body, request());
    store
        .begin_with_request(initial_events()[0].clone(), &restored)
        .unwrap();
    assert!(matches!(
        content
            .project(&store.snapshot(&request().turn_id).unwrap())
            .unwrap()
            .request,
        ContentResolution::Available { .. }
    ));
}

#[test]
fn missing_evidence_and_unbound_fixture_request_are_reported_without_invention() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let another_dir = tempfile::tempdir().unwrap();
    let absent =
        ExecutionContentStore::open(another_dir.path(), store.identity().unwrap()).unwrap();
    assert!(matches!(
        absent.project(&snapshot).unwrap().request,
        ContentResolution::Missing { .. }
    ));
    let other_root = tempfile::tempdir().unwrap();
    let (_, mut fixture_store) = canonical(&other_root);
    fixture_store.append(initial_events()[0].clone()).unwrap();
    let fixture_dir = tempfile::tempdir().unwrap();
    let fixture_content =
        ExecutionContentStore::open(fixture_dir.path(), fixture_store.identity().unwrap()).unwrap();
    assert!(matches!(
        fixture_content
            .project(&fixture_store.snapshot(&request().turn_id).unwrap())
            .unwrap()
            .request,
        ContentResolution::NotRecorded
    ));
}

#[test]
fn accepted_output_resolves_exact_body_and_preserves_unknown_usage() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let partial = output(&snapshot, OutputKind::Partial);
    content.retain_output(&snapshot, partial.clone()).unwrap();
    let final_output = output(&snapshot, OutputKind::Final);
    let retained = content
        .retain_output(&snapshot, final_output.clone())
        .unwrap();
    let activation = retained.activation().clone();
    store
        .append(event(
            &snapshot,
            "accept",
            TurnContractEvent::AcceptActivation {
                activation: activation.clone(),
                checkpoint: Box::new(CheckpointRef {
                    checkpoint_id: CheckpointId::new("candidate-a").unwrap(),
                    session_id: owner().session_id,
                    conversation_id: NodeConversationId::new("conversation-a").unwrap(),
                    source: CheckpointSource::Accepted { activation },
                }),
                output: retained.reference().clone(),
            },
        ))
        .unwrap();
    let accepted = store.snapshot(&request().turn_id).unwrap();
    store
        .append(event(
            &accepted,
            "completed",
            TurnContractEvent::Close {
                closure: TurnClosure::Completed,
            },
        ))
        .unwrap();
    let view = content
        .project(&store.snapshot(&request().turn_id).unwrap())
        .unwrap();
    assert_eq!(view.state, LogicalTurnState::Completed);
    assert!(view.activations[0].currently_accepted);
    assert_eq!(view.activations[0].partial_outputs, vec![partial]);
    assert!(
        matches!(&view.activations[0].output, ContentResolution::Available { content, .. } if *content == final_output)
    );
    let missing_dir = tempfile::tempdir().unwrap();
    let missing =
        ExecutionContentStore::open(missing_dir.path(), store.identity().unwrap()).unwrap();
    assert!(matches!(
        missing
            .project(&store.snapshot(&request().turn_id).unwrap())
            .unwrap()
            .activations[0]
            .output,
        ContentResolution::Missing { .. }
    ));
}

#[test]
fn output_for_an_unretained_generation_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let mut body = output(&snapshot, OutputKind::Final);
    body.activation.generation += 1;
    assert!(matches!(
        content.retain_output(&snapshot, body),
        Err(ExecutionContentError::OwnerMismatch)
    ));
}

#[test]
fn protected_tool_bytes_reserve_settlement_and_late_truncated_result_is_explicit() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let identity = store.identity().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
    let snapshot = begin_running(&mut store, &mut content);
    let activation = &snapshot.contract().activations()[0].activation;
    let invocation = InvocationId::new("tool-a").unwrap();
    let arguments = content
        .reserve_tool(
            &snapshot,
            activation,
            invocation.clone(),
            b"\x00\xff{exact}",
            4,
        )
        .unwrap();
    assert_eq!(
        content.read_tool_arguments(&arguments).unwrap(),
        b"\x00\xff{exact}"
    );
    assert_eq!(arguments.protected_arguments().byte_len, 9);
    assert_eq!(
        arguments,
        content
            .reserve_tool(
                &snapshot,
                activation,
                invocation.clone(),
                b"\x00\xff{exact}",
                4
            )
            .unwrap()
    );
    assert!(matches!(
        content.reserve_tool(&snapshot, activation, invocation.clone(), b"different", 4),
        Err(ExecutionContentError::Conflict)
    ));
    store
        .append(event(
            &snapshot,
            "interrupted",
            TurnContractEvent::InterruptEpoch {
                epoch_id: activation.execution_epoch_id.clone(),
            },
        ))
        .unwrap();
    let interrupted = store.snapshot(&request().turn_id).unwrap();
    store
        .append(event(
            &interrupted,
            "cancelled",
            TurnContractEvent::Close {
                closure: TurnClosure::Cancelled,
            },
        ))
        .unwrap();
    drop(content);
    let mut content = ExecutionContentStore::open(dir.path(), identity).unwrap();
    let restored = content
        .tool_arguments(
            &store.snapshot(&request().turn_id).unwrap(),
            activation,
            &invocation,
        )
        .unwrap()
        .unwrap();
    assert_eq!(restored, arguments);
    let result = content
        .record_tool_result(&restored, InvocationOutcome::Succeeded, b"abcdefghij", 100)
        .unwrap();
    assert!(result.is_truncated());
    assert_eq!(result.original_byte_len(), 10);
    assert_eq!(content.read_tool_result(&result).unwrap(), b"abcd");
    assert_eq!(content.tool_result(&arguments).unwrap().unwrap(), result);
    assert!(matches!(
        content.record_tool_result(&arguments, InvocationOutcome::Failed, b"different", 101),
        Err(ExecutionContentError::Conflict)
    ));
}

#[test]
fn observed_success_and_error_survive_truncation_with_full_raw_digest_after_reopen() {
    use sha2::{Digest, Sha256};
    for (status, raw) in [
        (
            InvocationOutcome::Succeeded,
            br#"{"Ok":{"result":"a long returned value"}}"#.as_slice(),
        ),
        (
            InvocationOutcome::Failed,
            br#"{"Err":"backend failed after a possible external effect"}"#.as_slice(),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (_, mut store) = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = store.identity().unwrap();
        let mut content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
        let snapshot = begin_running(&mut store, &mut content);
        let activation = &snapshot.contract().activations()[0].activation;
        let arguments = content
            .reserve_tool(
                &snapshot,
                activation,
                InvocationId::new("tool-a").unwrap(),
                b"{}",
                2,
            )
            .unwrap();
        let result = content
            .record_tool_result(&arguments, status, raw, 100)
            .unwrap();
        assert!(result.is_truncated());
        assert_eq!(result.outcome(), status);
        assert_eq!(
            result.original_sha256(),
            format!("{:x}", Sha256::digest(raw))
        );
        assert_ne!(result.original_sha256(), result.protected_result().sha256);
        assert_eq!(result.original_byte_len(), raw.len() as u64);
        assert_eq!(content.read_tool_result(&result).unwrap(), b"{\"");
        // The retained prefix is intentionally not a parseable full result.
        assert!(serde_json::from_slice::<serde_json::Value>(
            &content.read_tool_result(&result).unwrap()
        )
        .is_err());
        let other_status = if status == InvocationOutcome::Succeeded {
            InvocationOutcome::Failed
        } else {
            InvocationOutcome::Succeeded
        };
        assert!(matches!(
            content.record_tool_result(&arguments, other_status, raw, 100),
            Err(ExecutionContentError::Conflict)
        ));
        drop(content);
        let content = ExecutionContentStore::open(dir.path(), identity).unwrap();
        let restored = content.tool_result(&arguments).unwrap().unwrap();
        assert_eq!(restored, result);
        assert_eq!(restored.outcome(), status);
        assert_eq!(
            restored.original_sha256(),
            format!("{:x}", Sha256::digest(raw))
        );
    }
}

fn legacy_begin(store: &mut SessionTurnStore, id: &str) {
    store
        .begin(BeginSessionTurn {
            turn_id: Some(id.into()),
            session_id: owner().session_id.as_str().into(),
            user_input: format!("request {id}"),
            agent_id: Some("coder".into()),
            model: Some("model-a".into()),
            context: vec![],
            idempotency_key: None,
            metadata: Default::default(),
        })
        .unwrap();
}
fn legacy_close(store: &mut SessionTurnStore, id: &str) {
    store
        .transition(
            id,
            format!("close-{id}"),
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
fn legacy_frontier_rejects_running_and_retains_superseded_rows_before_sealing() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let mut legacy = SessionTurnStore::open(root.path().join("session-history")).unwrap();
    legacy_begin(&mut legacy, "old-a");
    assert!(matches!(
        content.retain_legacy_history(&store.legacy_history_snapshot().unwrap()),
        Err(ExecutionContentError::Invalid(_))
    ));
    legacy_close(&mut legacy, "old-a");
    legacy_begin(&mut legacy, "old-b");
    legacy_close(&mut legacy, "old-b");
    legacy
        .rewind(owner().session_id.as_str(), Some("old-a"), "rewind-b")
        .unwrap();
    let receipt = content
        .retain_legacy_history(&store.legacy_history_snapshot().unwrap())
        .unwrap();
    assert_eq!(receipt.last_predecessor().unwrap().turn_id, "old-a");
    assert_eq!(content.legacy_history(&receipt).unwrap().turns.len(), 2);
    assert!(content.legacy_history(&receipt).unwrap().turns[1].superseded);
    let seal = store.seal_legacy_history(&receipt).unwrap();
    assert_eq!(
        content.read_legacy_history(&seal).unwrap().turns,
        legacy.list_including_superseded("session-a")
    );
    let snapshot = begin_running(&mut store, &mut content);
    assert_eq!(
        content
            .project(&snapshot)
            .unwrap()
            .legacy_predecessor
            .unwrap()
            .turn_id,
        "old-a"
    );
    legacy_begin(&mut legacy, "old-c");
    legacy_close(&mut legacy, "old-c");
    assert!(store.legacy_history_snapshot().is_err());
}

#[test]
fn unknown_schema_and_changed_body_digest_fail_closed_on_reopen() {
    for mutation in ["schema", "body"] {
        let root = tempfile::tempdir().unwrap();
        let (_, store) = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = store.identity().unwrap();
        let mut content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
        content.retain_request(request()).unwrap();
        drop(content);
        if mutation == "schema" {
            let file = dir.path().join("execution-content.v1.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            value["schema_version"] = 99.into();
            fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        } else {
            // Records live in the active segment, one JSON line each after
            // the segment header.
            let file = dir.path().join("execution-content.active.jsonl");
            let text = fs::read_to_string(&file).unwrap();
            let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
            let mut value: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
            value["record"]["body"]["display_input"] = "tampered".into();
            lines[1] = serde_json::to_string(&value).unwrap();
            fs::write(&file, lines.join("\n") + "\n").unwrap();
        }
        assert!(ExecutionContentStore::open(dir.path(), identity).is_err());
    }
}

#[test]
fn owned_content_requires_exact_component_and_retains_parent_locks() {
    let root = tempfile::tempdir().unwrap();
    let (guard, store) = canonical(&root);
    let wrong = store
        .component_namespace(ExecutionComponent::ActivationState)
        .unwrap();
    assert!(ExecutionContentStore::open_owned(wrong).is_err());
    let namespace = store
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    let mut content = ExecutionContentStore::open_owned(namespace).unwrap();
    assert!(store
        .component_namespace(ExecutionComponent::ExecutionContent)
        .is_err());
    drop(store);
    drop(guard);
    assert!(LegacyFormatOwnership::acquire(root.path()).is_err());
    content.retain_request(request()).unwrap();
}

#[test]
fn missing_owned_primary_cannot_reset_retained_content_with_only_marker_remaining() {
    let root = tempfile::tempdir().unwrap();
    let (_, store) = canonical(&root);
    let namespace = store
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    let mut content = ExecutionContentStore::open_owned(namespace).unwrap();
    let receipt = content.retain_request(request()).unwrap();
    drop(content);
    let content_root = store.path().parent().unwrap().join("execution-content");
    let primary = content_root.join("execution-content.v1.json");
    let original = fs::read(&primary).unwrap();
    fs::remove_file(&primary).unwrap();
    // The initialization marker and the record log remain.
    let mut remaining: Vec<String> = fs::read_dir(&content_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    remaining.sort();
    assert_eq!(remaining.len(), 2, "{remaining:?}");
    assert!(remaining.contains(&"execution-content.active.jsonl".to_string()));
    let namespace = store
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    assert!(ExecutionContentStore::open_owned(namespace).is_err());
    assert!(!primary.exists());
    // The fixture restores known exact bytes; the opener never fabricates them.
    fs::write(&primary, original).unwrap();
    let namespace = store
        .component_namespace(ExecutionComponent::ExecutionContent)
        .unwrap();
    let content = ExecutionContentStore::open_owned(namespace).unwrap();
    assert_eq!(
        content
            .retained_request(&request().turn_id)
            .unwrap()
            .unwrap()
            .0,
        receipt
    );
}

#[test]
fn previously_unmarked_content_acquires_marker_only_after_validating_primary() {
    for valid in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (_, store) = canonical(&root);
        let dir = tempfile::tempdir().unwrap();
        let identity = store.identity().unwrap();
        let content = ExecutionContentStore::open(dir.path(), identity.clone()).unwrap();
        drop(content);
        let marker = dir.path().join(".journal-initialized.v1.json");
        fs::remove_file(&marker).unwrap();
        if !valid {
            fs::write(dir.path().join("execution-content.v1.json"), b"corrupt").unwrap();
        }
        let opened = ExecutionContentStore::open(dir.path(), identity);
        assert_eq!(opened.is_ok(), valid);
        assert_eq!(marker.exists(), valid);
    }
}

fn resolved_input_fixture(
    store: &mut SessionExecutionStore,
    content: &mut ExecutionContentStore,
    wrong_guidance_role: bool,
) -> (DurableTurnSnapshot, AuthorityGrant) {
    let limits = GrantLimits {
        activations: 2,
        invocations: 3,
        tokens: 4000,
        cost_microunits: 5000,
    };
    let profile = ExecutionProfile {
        definition: "shared-coder".into(),
        provider: "provider-a".into(),
        model: "model-a".into(),
        isolation: "local-podman".into(),
        tools: vec!["read_file".into()],
        write_scope: None,
    };
    let policy = AuthorityGrant {
        id: "supervisor-grant".into(),
        revision: 1,
        issuer_evidence: EvidenceRef::new("user-grant-approval").unwrap(),
        holder: TurnNodeId::new("node-a").unwrap(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![profile.clone()],
        limits: limits.clone(),
        expires_at_ms: 1789999999999,
    };
    let definition = content
        .retain_activation_evidence(ActivationEvidenceContent::Definition {
            definition_id: AgentDefinitionId::new("shared-coder").unwrap(),
            revision: 7,
            profile,
            configuration: "{\"system_prompt\":\"inspect exactly\"}".into(),
        })
        .unwrap();
    let grant = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    let budget = content
        .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
        .unwrap();
    let attachment = content
        .retain_activation_evidence(ActivationEvidenceContent::Attachment {
            reference_id: "attachment-a".into(),
            media_type: "text/plain".into(),
            text: "immutable source bytes".into(),
        })
        .unwrap();
    let repository = content
        .retain_activation_evidence(ActivationEvidenceContent::Repository {
            description: "recorded checkout, availability separately checked".into(),
            revision: Some("commit-a".into()),
        })
        .unwrap();
    let request = content.retain_request(request()).unwrap();
    let mut events = initial_events();
    let TurnContractEvent::Begin { graph, .. } = &mut events[0].event else {
        panic!("Begin")
    };
    graph.nodes[0].definition.snapshot = definition.reference().clone();
    let TurnContractEvent::StartActivation { input } = &mut events[1].event else {
        panic!("Start")
    };
    input.definition.snapshot = definition.reference().clone();
    input.guidance = vec![if wrong_guidance_role {
        budget.reference().clone()
    } else {
        request.reference().clone()
    }];
    input.attachments = vec![attachment.reference().clone()];
    input.repository = RepositoryInput::Recorded {
        snapshot: repository.reference().clone(),
    };
    input.budget = budget.reference().clone();
    input.grant.as_mut().unwrap().evidence = grant.reference().clone();
    store
        .begin_with_request(events[0].clone(), &request)
        .unwrap();
    store.append(events[1].clone()).unwrap();
    (store.snapshot(request.turn_id()).unwrap(), policy)
}

#[test]
fn canonical_manifest_resolves_exact_typed_definition_grant_budget_and_request_guidance() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let (snapshot, grant) = resolved_input_fixture(&mut store, &mut content, false);
    let manifest = &snapshot.contract().activations()[0].input;
    let resolved = content.validate_input(&snapshot, manifest).unwrap();
    assert_eq!(resolved.grant, Some(grant.clone()));
    assert_eq!(resolved.budget, grant.limits);
    assert_eq!(resolved.guidance, vec![request().effective_input]);
    assert!(matches!(
        resolved.definition,
        ActivationEvidenceContent::Definition { revision: 7, .. }
    ));
    assert!(
        matches!(&resolved.attachments[0], ActivationEvidenceContent::Attachment { text, .. } if text == "immutable source bytes")
    );
    let mut unrecorded = manifest.clone();
    unrecorded.grant.as_mut().unwrap().revision += 1;
    assert!(matches!(
        content.validate_input(&snapshot, &unrecorded),
        Err(ExecutionContentError::Conflict)
    ));
    drop(content);
    let content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    assert_eq!(
        content.validate_input(&snapshot, manifest).unwrap().grant,
        Some(grant)
    );
}

#[test]
fn existing_evidence_with_wrong_semantic_role_cannot_satisfy_activation_input() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut store) = canonical(&root);
    let dir = tempfile::tempdir().unwrap();
    let mut content = ExecutionContentStore::open(dir.path(), store.identity().unwrap()).unwrap();
    let (snapshot, _) = resolved_input_fixture(&mut store, &mut content, true);
    assert!(matches!(
        content.validate_input(&snapshot, &snapshot.contract().activations()[0].input),
        Err(ExecutionContentError::Invalid(
            "missing or wrong role guidance evidence"
        ))
    ));
}
