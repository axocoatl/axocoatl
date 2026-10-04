use axocoatl_session::invocation_audit::{
    InvocationAudit, InvocationAuditError, InvocationAuditOwner, InvocationEvidenceCommand,
    InvocationFinalEvidence, InvocationIntentCommand, InvocationOutcomeSource, ProtectedArguments,
};
use axocoatl_session::turn_contract::{
    CommandId, EffectDisposition, EvidenceRef, InvocationOutcome, LogicalTurnState, SessionId,
    TurnClosure, TurnContract, TurnContractEnvelope, TurnContractEvent,
    TURN_CONTRACT_SCHEMA_VERSION,
};

fn intent() -> InvocationIntentCommand {
    serde_json::from_str(include_str!("fixtures/invocation-audit-intent.v1.json")).unwrap()
}

fn owner() -> InvocationAuditOwner {
    InvocationAuditOwner {
        workspace_id: "workspace-a".into(),
        session_id: SessionId::new("session-a").unwrap(),
    }
}

fn outcome(intent: &InvocationIntentCommand) -> InvocationEvidenceCommand {
    InvocationEvidenceCommand {
        command_id: CommandId::new("outcome-command-a").unwrap(),
        expected_revision: 1,
        invocation_id: intent.intent.invocation_id.clone(),
        activation: intent.intent.activation.clone(),
        authority: intent.intent.authority.clone(),
        evidence: InvocationFinalEvidence::Outcome {
            outcome: InvocationOutcome::Succeeded,
            result: ProtectedArguments {
                evidence_ref: EvidenceRef::new("protected-result-a").unwrap(),
                sha256: "a".repeat(64),
                byte_len: 128,
            },
            redacted_preview: "completed with retained result".into(),
            source: InvocationOutcomeSource::Executor,
            authority_ref: EvidenceRef::new("verified-adapter-observation-a").unwrap(),
        },
    }
}

#[test]
fn durable_intent_identity_survives_restart_without_becoming_replay_authority() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let command = intent();
    let receipt = audit.record_intent(command.clone()).unwrap();
    assert_eq!(receipt.invocation_id(), &command.intent.invocation_id);
    assert_eq!(receipt.activation(), &command.intent.activation);
    assert_eq!(receipt.intent(), &command.intent);
    assert_eq!(receipt.revision(), 1);
    assert!(audit.is_dispatchable_receipt(&receipt).unwrap());
    let original = audit.records().unwrap().to_vec();
    audit.record_intent(command.clone()).unwrap();
    assert_eq!(audit.records().unwrap(), original);
    drop(audit);

    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    assert_eq!(audit.records().unwrap(), original);
    assert!(audit.is_dispatchable_receipt(&receipt).unwrap());
    assert_eq!(
        audit
            .invocation(receipt.invocation_id())
            .unwrap()
            .unwrap()
            .disposition(),
        EffectDisposition::OutcomeUnknown
    );
    let mut new_scope = command.clone();
    new_scope.command_id = CommandId::new("new-scope-command").unwrap();
    new_scope.intent.dispatch_scope = "new-live-dispatch-scope".into();
    assert!(matches!(
        audit.record_intent(new_scope),
        Err(InvocationAuditError::IntentConflict)
    ));
    assert_eq!(audit.records().unwrap(), original);
    // Persistence proof remains historical; the live control arbiter must reject
    // this old dispatch scope. Reopening itself cannot create a new invocation.
    assert_eq!(
        audit
            .record_intent(command)
            .unwrap()
            .intent()
            .dispatch_scope,
        "live-dispatch-scope-a"
    );
}

#[test]
fn late_outcome_settles_unknown_without_reopening_or_rewriting_closed_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let command = intent();
    let receipt = audit.record_intent(command.clone()).unwrap();
    let mut turn = TurnContract::default();
    let mut envelope = TurnContractEnvelope {
        schema_version: TURN_CONTRACT_SCHEMA_VERSION,
        command_id: CommandId::new("begin-turn").unwrap(),
        expected_revision: 0,
        session_id: command.intent.activation.session_id.clone(),
        turn_id: command.intent.activation.turn_id.clone(),
        event: TurnContractEvent::Begin {
            epoch_id: command.intent.activation.execution_epoch_id.clone(),
            predecessor: None,
            graph: serde_json::from_value(serde_json::json!({
                "snapshot_id":"initial-graph", "revision":1,
                "nodes":[{
                    "node_id":command.intent.activation.node_id,
                    "slot_id":"slot-a",
                    "definition":{"definition_id":"coder", "snapshot":"coder-v1"},
                    "conversation_id":"conversation-a", "starting_savepoint":{"kind":"empty"},
                    "required":true,
                }],
                "dependencies":[], "conditions":[],
            }))
            .unwrap(),
        },
    };
    turn.apply(&envelope).unwrap();
    envelope.command_id = CommandId::new("cancel-turn").unwrap();
    envelope.expected_revision = 1;
    envelope.event = TurnContractEvent::Close {
        closure: TurnClosure::Cancelled,
    };
    turn.apply(&envelope).unwrap();
    let closed = turn.clone();
    let initial_record = audit.records().unwrap()[0].clone();
    drop(audit);

    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let observed = outcome(&command);
    let acknowledged = audit.record_evidence(observed.clone()).unwrap();
    assert_eq!(acknowledged.invocation_id(), receipt.invocation_id());
    assert_eq!(acknowledged.revision(), 2);
    assert_eq!(audit.records().unwrap()[0], initial_record);
    assert_eq!(audit.records().unwrap().len(), 2);
    audit.record_evidence(observed).unwrap();
    assert_eq!(audit.records().unwrap().len(), 2);
    assert!(!audit.is_dispatchable_receipt(&receipt).unwrap());
    assert_eq!(turn, closed);
    assert_eq!(turn.state(), Some(LogicalTurnState::Cancelled));
    drop(audit);
    let audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    assert_eq!(
        audit
            .invocation(receipt.invocation_id())
            .unwrap()
            .unwrap()
            .disposition(),
        EffectDisposition::OutcomeRecorded
    );
    assert_eq!(turn, closed);
}

#[test]
fn failed_tool_result_never_proves_no_effect_and_cannot_be_rewritten_as_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let command = intent();
    audit.record_intent(command.clone()).unwrap();
    let mut failed = outcome(&command);
    if let InvocationFinalEvidence::Outcome { outcome, .. } = &mut failed.evidence {
        *outcome = InvocationOutcome::Failed;
    }
    audit.record_evidence(failed.clone()).unwrap();
    let before = audit.records().unwrap().to_vec();
    assert_eq!(
        audit
            .invocation(&command.intent.invocation_id)
            .unwrap()
            .unwrap()
            .disposition(),
        EffectDisposition::OutcomeRecorded
    );
    let mut contradiction = failed;
    contradiction.command_id = CommandId::new("contradictory-proof").unwrap();
    contradiction.expected_revision = 2;
    contradiction.evidence = InvocationFinalEvidence::NotDispatched {
        evidence: EvidenceRef::new("claimed-no-dispatch").unwrap(),
        authority_ref: EvidenceRef::new("claimed-dispatch-gate-proof").unwrap(),
    };
    assert!(matches!(
        audit.record_evidence(contradiction),
        Err(InvocationAuditError::EvidenceConflict)
    ));
    assert_eq!(audit.records().unwrap(), before);
}

#[test]
fn positive_pre_dispatch_cancellation_is_retained_and_prevents_dispatch_claim() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let command = intent();
    let receipt = audit.record_intent(command.clone()).unwrap();
    let mut cancelled = outcome(&command);
    cancelled.evidence = InvocationFinalEvidence::NotDispatched {
        evidence: EvidenceRef::new("cancelled-before-dispatch").unwrap(),
        authority_ref: EvidenceRef::new("verified-live-gate-decision").unwrap(),
    };
    audit.record_evidence(cancelled).unwrap();
    assert!(!audit.is_dispatchable_receipt(&receipt).unwrap());
    assert_eq!(
        audit
            .invocation(receipt.invocation_id())
            .unwrap()
            .unwrap()
            .disposition(),
        EffectDisposition::NotDispatched
    );
}

#[test]
fn stale_conflicting_and_cross_owner_evidence_is_rejected_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let command = intent();
    audit.record_intent(command.clone()).unwrap();
    let original = audit.records().unwrap().to_vec();
    let mut conflict = command.clone();
    conflict.intent.redacted_preview = "changed display must not rewrite original intent".into();
    assert!(matches!(
        audit.record_intent(conflict),
        Err(InvocationAuditError::CommandConflict)
    ));

    let mut stale = outcome(&command);
    stale.expected_revision = 0;
    assert!(matches!(
        audit.record_evidence(stale),
        Err(InvocationAuditError::StaleRevision { actual: 1, .. })
    ));
    for field in [
        "session_id",
        "turn_id",
        "execution_epoch_id",
        "node_id",
        "activation_id",
        "generation",
        "grant_id",
        "grant_revision",
        "approval_ref",
    ] {
        let mut value = serde_json::to_value(outcome(&command)).unwrap();
        match field {
            "generation" => value["activation"][field] = 2.into(),
            "grant_revision" => value["authority"][field] = 4.into(),
            "grant_id" | "approval_ref" => value["authority"][field] = "other-owner".into(),
            _ => value["activation"][field] = "other-owner".into(),
        }
        let forged = serde_json::from_value(value).unwrap();
        assert!(
            audit.record_evidence(forged).is_err(),
            "accepted forged {field}"
        );
        assert_eq!(audit.records().unwrap(), original);
    }
    let observed = outcome(&command);
    audit.record_evidence(observed.clone()).unwrap();
    let settled = audit.records().unwrap().to_vec();
    let mut conflicting_observation = observed;
    if let InvocationFinalEvidence::Outcome { authority_ref, .. } =
        &mut conflicting_observation.evidence
    {
        *authority_ref = EvidenceRef::new("different-observer").unwrap();
    }
    assert!(matches!(
        audit.record_evidence(conflicting_observation),
        Err(InvocationAuditError::CommandConflict)
    ));
    assert_eq!(audit.records().unwrap(), settled);
}

#[test]
fn receipts_from_another_audit_cannot_certify_this_store_even_with_identical_intent() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let mut first = InvocationAudit::open(first_dir.path(), owner()).unwrap();
    let mut second = InvocationAudit::open(second_dir.path(), owner()).unwrap();
    let first_receipt = first.record_intent(intent()).unwrap();
    let second_receipt = second.record_intent(intent()).unwrap();
    assert!(first.is_dispatchable_receipt(&first_receipt).unwrap());
    assert!(!first.is_dispatchable_receipt(&second_receipt).unwrap());
    assert!(!second.is_dispatchable_receipt(&first_receipt).unwrap());
    drop(first);
    let mut wrong_owner = owner();
    wrong_owner.workspace_id = "another-client".into();
    assert!(matches!(
        InvocationAudit::open(first_dir.path(), wrong_owner),
        Err(InvocationAuditError::OwnerConflict)
    ));
}

#[test]
fn protected_arguments_are_distinct_from_preview_and_invalid_metadata_cannot_acknowledge() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    let command = intent();
    for field in [
        "digest",
        "size",
        "preview",
        "generation",
        "scope",
        "authority",
    ] {
        let mut invalid = command.clone();
        match field {
            "digest" => invalid.intent.arguments.sha256 = "a display-only summary".into(),
            "size" => invalid.intent.arguments.byte_len = u64::MAX,
            "preview" => invalid.intent.redacted_preview = "x".repeat(2049),
            "generation" => invalid.intent.activation.generation = 0,
            "scope" => invalid.intent.dispatch_scope = "../another-gate".into(),
            "authority" => invalid.intent.authority.grant_revision = 0,
            _ => unreachable!(),
        }
        assert!(
            audit.record_intent(invalid).is_err(),
            "accepted invalid {field}"
        );
        assert!(audit.records().unwrap().is_empty());
    }
    let receipt = audit.record_intent(command.clone()).unwrap();
    assert_eq!(receipt.intent().arguments, command.intent.arguments);
    assert_ne!(
        receipt.intent().arguments.evidence_ref.as_str(),
        receipt.intent().redacted_preview
    );
    let mut altered = command;
    altered.intent.arguments.evidence_ref = EvidenceRef::new("other-protected-bytes").unwrap();
    assert!(matches!(
        audit.record_intent(altered),
        Err(InvocationAuditError::CommandConflict)
    ));
}

/// The active segment of the audit's log, beside its head file.
fn active_segment(audit: &InvocationAudit) -> std::path::PathBuf {
    audit
        .path()
        .parent()
        .unwrap()
        .join("invocation-audit.active.jsonl")
}

#[test]
fn corrupted_or_future_journals_fail_closed_without_rewriting_their_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    audit.record_intent(intent()).unwrap();
    let head_path = audit.path();
    let active_path = active_segment(&audit);
    drop(audit);
    let head: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&head_path).unwrap()).unwrap();
    let active = String::from_utf8(std::fs::read(&active_path).unwrap()).unwrap();
    let lines: Vec<&str> = active.lines().collect();
    assert_eq!(lines.len(), 2, "a header and one record");
    let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    for kind in [
        "future",
        "duplicate",
        "sequence",
        "revision",
        "unknown_field",
        "owner",
    ] {
        let mut corrupted_head = head.clone();
        let mut corrupted = record.clone();
        let mut extra = None;
        match kind {
            "future" => corrupted_head["schema_version"] = 99.into(),
            "duplicate" => {
                let mut repeat = record.clone();
                repeat["record"]["sequence"] = 2.into();
                extra = Some(repeat);
            }
            "sequence" => corrupted["record"]["sequence"] = 3.into(),
            "revision" => corrupted["record"]["invocation_revision"] = 2.into(),
            "unknown_field" => corrupted["record"]["hidden_authority"] = true.into(),
            "owner" => corrupted_head["owner"]["workspace_id"] = "other-workspace".into(),
            _ => unreachable!(),
        }
        let head_bytes = serde_json::to_vec(&corrupted_head).unwrap();
        let mut active_bytes = format!("{}\n{}\n", lines[0], corrupted);
        if let Some(extra) = extra {
            active_bytes.push_str(&format!("{extra}\n"));
        }
        std::fs::write(&head_path, &head_bytes).unwrap();
        std::fs::write(&active_path, &active_bytes).unwrap();
        assert!(
            InvocationAudit::open(dir.path(), owner()).is_err(),
            "accepted {kind}"
        );
        assert_eq!(std::fs::read(&head_path).unwrap(), head_bytes);
        assert_eq!(
            std::fs::read(&active_path).unwrap(),
            active_bytes.as_bytes()
        );
    }
    std::fs::write(&head_path, b"{partial").unwrap();
    assert!(InvocationAudit::open(dir.path(), owner()).is_err());
    assert_eq!(std::fs::read(head_path).unwrap(), b"{partial");
}

#[cfg(unix)]
#[test]
fn ownership_lock_and_failed_storage_never_issue_dispatch_acknowledgements() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    for path in [audit.path(), active_segment(&audit)] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(InvocationAudit::open(dir.path(), owner()).is_err());
    let active = active_segment(&audit);
    let snapshot = std::fs::read(&active).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"untouched").unwrap();
    std::fs::remove_file(&active).unwrap();
    symlink(outside.path(), &active).unwrap();
    assert!(matches!(
        audit.record_intent(intent()),
        Err(InvocationAuditError::Io(_))
    ));
    assert!(matches!(
        audit.record_intent(intent()),
        Err(InvocationAuditError::RecoveryRequired)
    ));
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"untouched");
    drop(audit);
    assert!(InvocationAudit::open(dir.path(), owner()).is_err());
    std::fs::remove_file(&active).unwrap();
    std::fs::write(&active, snapshot).unwrap();
    let mut audit = InvocationAudit::open(dir.path(), owner()).unwrap();
    audit.record_intent(intent()).unwrap();
    assert_eq!(audit.records().unwrap().len(), 1);
}

#[test]
fn missing_control_directory_is_not_implicitly_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unprovisioned").join("audit");
    assert!(InvocationAudit::open(&path, owner()).is_err());
    assert!(!path.parent().unwrap().exists());
}
